---
created: 2026-09-20
updated: 2026-09-20
status: local-macos-verified-full-release-blocked
working_name: AI Account Manager
scope: standalone-desktop-application
first_platform: macOS-arm64
later_platform: Windows
impact_score: 24
implementation_status: implemented-with-provider-and-distribution-gates
---

# AI Account Manager 구현 스펙

> **2026-09-27 제거 기록.** 아래 본문 중 관리 OMP(`aam-omp` 18.2.6 sidecar·`integrations/omp/hard-control`·OMP 계정 고정 실행·관리 `omp` shim·`aam run omp`)와 Grok(`grok`)·Antigravity(`agy`) CLI 어댑터의 실행·등록·로그인 경로, RPC `account.verify`는 이 날짜에 코드에서 삭제했다. 이유: 관리 OMP는 omp 설정에 `auth.broker`가 있으면 항상 잠기는데(`hard-control/bootstrap.ts`) AAM 계정 브릿지가 broker를 요구하므로 영구히 실행 불가능했고, Grok·agy는 identity·격리 gate로 한 번도 실행 경로에 도달하지 못했다. 이제 omp·Grok·Antigravity 계정은 `docs/specs/2026-09-26-aam-account-bridge.md`의 브릿지(omp `/login` → AAM 공급자 `aam-claude`·`aam-grok`·`aam-antigravity` 등)로만 배정하며, AAM이 직접 실행·관리하는 CLI는 Claude Code·Codex다. `omp usage --json` 관측, 관측 계정 행(tool `omp`), 외부 OMP 세션 관측, Claude 대화 인계는 유지한다. 이하 관련 서술은 당시 기록으로 남긴다.

## 1. 결론과 합의된 목표

Orca·Terminal·iTerm·VS Code·스크립트에서 실행하는 AI CLI를 사용자 단위 공통 관리자로 연결한다. macOS 설치형 앱을 먼저 제공하고 Windows 설치형 앱으로 확장한다. 계정별 현황을 한 화면에 표시하고, 새 작업의 자동 계정 배정과 명시적인 수동 전환을 실제 적용 계정 확인까지 수행한다.

사용자가 승인한 방향은 공통 관리자 + 도구별 launcher/adapter다. 설치형 GUI, macOS 우선, Windows 후속, 한눈에 보는 계정 현황과 신뢰할 수 있는 배정을 목표로 한다. 이 저장소에 Mac 앱·서비스·launcher를 구현했다. 이 문서의 전체 요구 범위는 유지하며, 실제 검증과 미통과 조건은 17절에 구분한다. 전체 공급자 지원 완료를 뜻하지 않는다.

**완료의 뜻:** 앱을 설치한 사용자가 계정을 등록하고, 두 종류 이상의 터미널에서 동시에 관리 대상 CLI를 시작하며, 올바른 계정 배정·상태 표시·수동 전환·종료 복구를 확인할 수 있어야 한다. 대시보드만 있거나 프로필 파일만 바뀌는 상태는 완료가 아니다. 요구된 공급자는 Anthropic, OpenAI, Google, xAI 모두이며 미지원 adapter가 남으면 전체 공급자 지원 완료라고 표시하지 않는다.

## 2. 제품 경계와 보장 수준

### 2.1 관리 범위

- 같은 OS 사용자로 실행하는 여러 터미널·앱이 하나의 관리 서비스를 공유한다. 다른 OS 사용자까지 관리하는 root 서비스는 아니다.
- 인증 토큰 만료, 구독 quota 리셋, 유료 크레딧 만료를 별도 개념으로 다룬다.
- 새 세션 배정 시 작업에 필요한 모델·계정 권한·quota·진행 중 작업을 고려한다.
- 기존 세션은 계정에 고정한다. 주기적인 quota 조회가 기존 세션의 인증을 바꾸지 않는다.
- 공식 CLI를 변경하지 않고 계정별 프로필로 실행하는 경로를 우선한다. 인증 토큰을 모아 범용 inference API로 제공하는 제품이 아니다.
- 사용자 요청 없는 추가 과금, 리셋 쿠폰 소비, 모델 하향 전환, 로그인 만료 우회, 계정 자동 생성은 금지한다.

### 2.2 정직하게 표시할 한계

- launcher를 우회한 절대 경로 실행, 브라우저 사용, 다른 기기의 사용은 선행 예약으로 통제하지 못한다. 공급자 전체 quota 관측에 반영될 수 있을 뿐이다.
- 세션 예약은 공급자 quota를 실제 확보하는 예약이 아니다. 입장 제어의 추정값이며 429나 외부 소비를 완전히 방지하지 못한다.
- launcher는 실행 후 모든 native 모델 요청을 가로채지 않는다. 모델 변경·내부 subagent 소비의 관측 정밀도는 adapter capability로 표시한다.
- 이미 실행된 도구 호출을 재생하거나 실행 중인 CLI의 계정을 몰래 교체하지 않는다.
- 다중 본인 계정 소유가 자동 pooling의 약관상 허용을 뜻하지 않는다. 공식 문서의 지원 범위와 정책 확인이 기능 활성화 조건이다.

## 3. 확인된 근거와 추가 조사 결과

### 3.1 설계 당시 로컬 관측

| 항목 | 확인 결과 | 증명하지 않는 것 |
|---|---|---|
| OMP | 18.2.6; usage와 dry-balance 존재 | 외부 native CLI에 대한 배정 통제 |
| Fable dry-run | claude-fable-5-1 대상으로 12/12 선택 성공 | 실추론 성공, failover, 서로 다른 터미널의 예약 조정 |
| Claude Code | 2.1.278; `claude auth status --json` 명령 도움말 확인 | 여러 계정의 실로그인·실제 동시 실행 |
| Codex | 0.154.0 | 두 계정의 동시 운영 |
| Grok | 1.0.5; 공식 Grok Build | 현재 quota 소진 계정의 정상 추론 |
| Google | `agy` 설치 및 도움말 확인. `gemini`는 현재 PATH에서 찾지 못함 | Gemini CLI 설치 유무 전체, agy 계정별 동시 격리 |
| Anthropic identity | usage JSON metadata의 3개 accountId와 3개 orgId는 각각 서로 다름. 원문 ID는 문서에 저장하지 않음 | 독립 billing/quota라는 최종 보증 |

앞서 두 계정의 표시 사용률이 같았지만 identity는 다르다. 같은 사용률만으로 공유 quota라고 합치지 않는다. 공급자가 제공하는 entitlement/bucket 식별자와 문서화된 범위를 근거로 공유 관계를 결정한다.

### 3.2 OMP 재사용 지점

- OMP 18.2.6 소스는 `headroomFraction / remainingHours` 기반 required-drain 순위를 구현한다. 5시간 사용률 85% 이상 계정 하향, 계정 자격·차단 상태, 관측 여부, 세션 stickiness도 적용한다.
- Fable 전용 주간 bucket을 인식한다. 이 계산을 별도 proxy에서 중복 구현하는 것으로 시작하지 않는다.
- broker 모드의 `OMP_AUTH_BROKER_ACCOUNT_POOL_FILE`은 provider별 identityKey allowlist다. provider 누락은 무제한, 빈 배열은 해당 OAuth 계정을 숨김, API key는 제한하지 않음, 시작 시 한 번 읽음.
- 이 pool은 신뢰하는 client의 라우팅 설정이지 보안 권한 경계가 아니다. raw broker snapshot/cache에는 더 넓은 credential 접근이 있을 수 있다.
- `omp --profile`은 auth·settings·sessions·cache의 별도 profile 경로다. broker 설정을 상속하면 로컬 profile 분리만으로 broker 계정이 제한되지는 않는다.
- 기존 실행 발견은 인증·배정과 별개다. 같은 사용자의 원본 OMP 실행 파일과 OS PID/birth/boot·조상 관계·cwd를 조회해 외부 관측으로 표시한다. 하위 worker는 별도 터미널 수에 더하지 않고, 계보가 확인된 세션 기록만 부모에 연결한다. 서비스 자신의 usage/version 조회는 제외하며 argv·환경변수는 읽지 않는다.
- 계정 연결은 해당 프로세스가 쓰기용으로 열어 둔 JSONL의 소유자·device/inode·세션 header를 검증한 뒤 v3 메타데이터만 추출한다. `credential_pin`과 `omp usage --json`의 provider/accountId/email/orgId/projectId 원본 값을 OMP의 SHA-256 규칙으로 대조한다. 본문·오류 원문·토큰은 수집·IPC·UI·진단 대상에서 제외하며 credential DB를 직접 조회하지 않는다.
- 파일 기록만으로 현재 branch를 확정하지 않는다. 동일 provider의 여러 pin 또는 여러 계정이 같은 참조에 대응하면 미확인으로 남긴다. 모델 설정, assistant 호출 결과, 보조 `model_usage`는 계정 기록과 별개로 표시하고 실패한 호출을 성공으로 간주하지 않는다.
- OMP 18.2.6의 공개 확장은 현재 branch와 `listOAuthAccounts(provider, persistedSessionId)`의 유일한 active 선택을 전달한다. inactive fallback은 사용하지 않는다. persisted session ID와 provider request session ID가 다를 수 있어 이 선택을 요청별 인증 계정으로 승격하지 않는다. `message_end`는 core pin 기록보다 먼저 발생하므로 호출 결과와 이후 pin 관측도 분리한다.
- 확장 snapshot도 검증된 writer FD·세션 계보·PID/birth에 연결될 때만 사용한다. 사용자 전용 디렉터리/파일, no-follow, 크기·시간·freshness 검증을 적용하고 오래되거나 종료된 확장은 파일 기록으로 되돌린다. 최초 파일 읽기는 회당 파일별 2 MiB/전체 8 MiB로 제한되므로 긴 세션은 여러 상태 조회 뒤 따라잡으며 그동안 부분 관측으로 표시한다.
- `omp collab list --json`의 현재 host 0개, broker `not_configured`, 기존 TUI용 RPC attach 부재는 변하지 않는다. 위의 메타데이터 관측은 전체 provider·subagent의 identity 고정이나 개별 요청의 실제 인증 계정 증명이 아니다.

### 3.3 공식 CLI 인증 격리

| Adapter | 근거가 있는 격리·관측 방식 | 구현 전 실제 검증 |
|---|---|---|
| Claude | `CLAUDE_CONFIG_DIR`는 credential 파일뿐 아니라 macOS Keychain entry도 디렉터리별로 분리. auth status JSON, statusline rate_limits 제공 | A 로그인 유지 중 B 로그인·logout이 A에 영향 없는지; 프로필과 실제 인증 identity 일치 |
| Codex | `CODEX_HOME`; file/keyring/auto/ephemeral credential store. app-server account/read 및 account/rateLimits/read | 선택한 store의 profile 격리, 두 app-server의 identity, 갱신 경쟁 |
| Grok | 공식 `GROK_HOME`이 config/auth/sessions 등을 분리. billing 수집은 공식 CLI/외부 구현 참고 | auth identity를 secret 없이 확인할 경로, billing adapter 호환성, leader socket 격리 |
| Google | 현재 개인 경로는 agy. 공식 문서는 keyring과 전역 settings 경로, /usage 등을 설명 | 프로세스별 auth/keyring/backend 격리 수단을 아직 확인하지 못함 |

**Google 주의:** 조사한 커뮤니티 agy-switch-account는 전역 credentials symlink와 keyring을 교체하며 macOS 미검증이라고 명시한다. 동시 세션용으로 채택하지 않는다. HOME만 바꾸면 격리된다는 가정도 금지한다. Google 지원 요구는 유지하며 G-GOOGLE gate를 해결하기 전 자동 multi-account switch를 제공하지 않는다.

### 3.4 영향도

Code 4 + Runtime 5 + UX 4 + Data 3 + External 5 + Test 3 = **24/30, large**. 점수는 이 신규 제품에 대한 설계 판단이며 기존 웹 프로젝트 영향도가 아니다. 주요 위험은 credential 혼선, 공유 quota 중복 계산, 예약 조기 해제, GUI의 잘못된 전환 성공 표시, native CLI 업데이트, 공급자 정책이다.

## 4. 기술 방향과 대안

| 대안 | 장점 | 단점 | 결정 |
|---|---|---|---|
| Swift-only macOS 앱 | native 통합 우수 | Windows에서 UI·서비스 상당 부분 재작성 | 미선택 |
| Electron + Node 서비스 | 익숙한 UI 생태계 | 설치·런타임 무게와 별도 프로세스 관리 | 미선택 |
| **Tauri 2 + Rust core + TypeScript/React UI** | 설치형 앱, macOS/Windows UI 재사용, native core/프로세스 처리, tray | macOS 프로세스·서명 통합을 별도 구현·검증 | **권장 결정** |

공통 도메인·scheduler·SQLite·IPC schema는 Rust로 구현한다. React UI는 Tauri typed command만 호출하며 credential·임의 shell 실행 권한을 갖지 않는다. 별도 macOS 전용 전체 구현 후 포팅이 아니라 플랫폼 경계를 처음부터 분리한다. 이번 단계에서는 Windows 바이너리나 설치기를 만들지 않는다.

### 4.1 구성

- `AI Account Manager.app`: Tauri 메인 대시보드와 메뉴바/tray. 앱 창 닫기와 서비스 종료는 다른 동작.
- `aam-service`: 사용자 단위 단일 writer 관리 서비스. macOS LaunchAgent가 소유. UI의 단순 child sidecar로만 두지 않는다.
- `aam`: 진단·프로필 관리·배정·launcher CLI.
- 관리형 shims: `claude`, `codex`, `grok`, `omp`, 검증된 Google CLI에 대한 작은 launcher. 원본 바이너리를 교체하지 않는다.
- `platform-macos`: launchd, Keychain 참조, Unix socket, PID 시작 시각·boot identity, 프로세스/TTY.
- `platform-windows`: 동일 interface의 후속 구현 경계. 현재 dummy 구현이나 성공하는 no-op을 만들지 않는다.

### 4.2 저장과 IPC

- 앱 상태: OS 표준 app-data 경로. macOS는 `~/Library/Application Support/AI Account Manager/`.
- SQLite WAL + foreign_keys. 서비스만 쓰기. UI/launcher는 DB 파일을 직접 쓰지 않는다.
- 런타임 socket은 사용자 private 짧은 경로를 생성하고 UDS 길이 제한 검증. parent 0700, socket 0600, peer UID 검증, lock file로 단일 서비스 보장.
- Windows는 사용자 SID DACL이 적용된 named pipe로 교체. core에 `/tmp`, UID, symlink, POSIX signal을 직접 박지 않는다.
- JSON frame은 length-prefixed, protocolVersion 필수, 최대 1MiB. 요청 ID와 deadline을 포함. unknown method/oversized frame 거부.
- 같은 UID의 악성 프로세스로부터 완전 격리한다고 주장하지 않는다. 사용자 계정이 신뢰 경계이며 이 제품은 임의 코드를 실행하는 agent에 대한 보안 sandbox가 아니다.
- credential 원문은 SQLite·UI state·로그·지원용 export에 저장하지 않는다. profile 참조와 검증된 identity metadata만 저장한다. OAuth 갱신은 공식 CLI 또는 기존 OMP broker가 맡는다.

## 5. UX: 계정 현황을 한눈에 보기

### 5.1 메인 화면

앱은 마케팅 페이지가 아니라 운영 도구다. 큰 통합 사용률 카드 대신 **계정 행 + quota bucket 열 + 리셋 타임라인**을 중심으로 배치한다.

```text
AI Account Manager       서비스 정상       관리 연결 4/5       [자동 배정 켜짐]
[전체] [Anthropic] [OpenAI] [Google] [xAI]           모델 [Fable]   [새로고침]

계정 / 플랜 / 조직    인증     5시간 남음    주간 남음    모델 전용    리셋까지   세션   배정
A / Max / 개인       정상        72%          55%        Fable 31%      8h       2    자동
B / Max / 업무       정상        88%          70%        Fable 64%      2d       1    고정
C / Pro              재로그인     —            —             —         —        0    제외

[선택한 계정 상세]  실제 관측 시각 / 데이터 출처 / 공유 bucket / 허용 프로젝트
[이 계정으로 새 세션]  [새 세션 기본 계정으로 고정]  [자동 배정에서 제외]

실행 중 세션        도구       모델        실제 계정       상태          동작
session-...          Claude     Fable       A / 확인됨      작업 중       [전환 준비]
```

위 수치는 레이아웃 예시이며 실제 데이터로 제공하지 않는다.

- provider 간 %를 합쳐 전체 잔여 토큰으로 표시하지 않는다. shared bucket은 한 번만 표시하고 공유 계정 badge를 붙인다.
- `unknown`, `stale`, `exhausted`, `auth-required`, `quota-unavailable`, `unmanaged`를 구분한다. 미확인 사용량은 0%나 정상 녹색이 아니다.
- 리셋 상대시간과 절대시각/시간대, 마지막 **upstream 관측** 시각을 동시에 볼 수 있다.
- 주간·5시간·Fable quota를 각각 표시. 계정별 단일 %로 축약하지 않는다.
- next-launch 기본 계정과 현재 세션 계정은 별도 영역에 표시한다.
- 연결 상태 화면에서 각 CLI의 설치 위치, 버전, launcher 경유 여부, identity probe, 지원 capability, 마지막 검증을 표시한다. 사용자가 source label을 지정하면 Orca/Terminal로 표시할 수 있지만 터미널 이름을 보안 판단에 쓰지 않는다.
- 스크린샷용 이메일/조직 마스킹, keyboard 탐색, VoiceOver label, 시스템 글꼴·크기, light/dark, reduced motion 지원.

### 5.2 화면 목록 (2026-09-27 `docs/design/aam-final.html` 기준)

1. 지금 사용 현황 (⌘1): "가장 많이 쓰는 곳" + 2–7위 순위, 공급자(Anthropic/OpenAI Codex/Google Antigravity/xAI/Z.AI)별 계정 행(사용량 막대, 여유량·소진 안내)과 모델 셀(요청 수·5분 단위 sparkline·프로젝트 태그). 요청 수는 데스크톱 `bridge_usage` 명령이 `logs/bridge.log`의 대화 요청(`turn=true`)을 5분 단위로 모은 값이며 기간은 15분/1시간/오늘이다. omp가 AAM 모델을 거치지 않고 원래 공급자로 직접 호출한 기록은 `계정 미확인` 행과 상단 알림으로 구분한다.
2. 세션 (⌘2): omp 브릿지 세션과 Claude Code·Codex 관리 세션을 한 표(프로젝트·도구·모델·계정·요청·마지막 사용)로 보여 준다. 관리 세션 행을 누르면 상세와 Claude 재개 동작이 나온다.
3. 배정 정책 (⌘3): 배정 방식, 안전 여유량, 계정 순서와 배정 포함(신원 기준으로 묶은 실행 가능·브릿지 계정만), 도구별 "다음 실행 기본 계정" 선택(Claude Code·Codex, `policy.update { preferredAccounts }`, "없음(자동 배정)" 포함), 프로젝트 규칙. 프로젝트 allowlist·동시 세션 상한 편집은 화면에서 제외했고 RPC는 유지한다.
4. 연결 (⌘4): omp(계정 저장소 → AAM 로그인 항목 → 계정 연결 3단계, 연결/연결 해제), Claude Code·Codex 계정 목록과 계정 추가, Orca·터미널 연결 파일, 관리 서비스와 진단 내보내기. OMP 관측 확장 설치·제거는 CLI(`aam omp-observer`)로만 제공한다.

사이드바 하단에는 서비스 상태, 자동 배정 스위치, 사용량 조회 시각, 개인정보 가림 토글만 둔다.

메뉴바는 서비스 상태, 긴급한 리셋/인증 오류, 현재 배정 계정, 대시보드 열기·자동 배정 일시정지만 제공한다. 복잡한 정책 편집을 작은 메뉴에 넣지 않는다.

UI 언어는 앱 시작 시 한 번 정한다. 프런트엔드는 `navigator.languages`(없으면 `navigator.language`) 첫 항목이 `ko*`면 한국어, 아니면 영어를 쓰고, 트레이 메뉴(Rust)는 `defaults read -g AppleLanguages` 첫 항목(실패 시 `LC_ALL`/`LC_MESSAGES`/`LANG`)으로 같은 기준을 적용한다. 화면 문자열은 `apps/desktop/src/i18n.ts`의 `en`/`ko` 사전(키 누락은 typecheck 실패)과 `t(key, params)`로만 쓰며, 날짜·상대 시간은 선택한 locale의 `Intl`로 만든다. 서비스·어댑터가 보내는 문장(account.reason, notice, ApiError.message, 배정 사유)은 번역하지 않고 그대로 보여 준다.

### 5.3 시각 방향

**사용자 확정 방향:** 시니어 UX 디자이너 관점에서 macOS에 자연스럽게 어울리는 현대적인 설치형 앱으로 구현한다. 웹 대시보드를 창 안에 넣은 느낌보다 Mac 앱의 조작 관례·정보 위계·완성도를 우선한다. 트렌디함은 장식의 양이 아니라 정돈된 밀도, 섬세한 상태 변화, 일관된 상호작용으로 구현한다.

- **창 구조:** macOS 창 제어 버튼과 자연스럽게 이어지는 titlebar/toolbar, 간결한 sidebar, 계정 표 중심의 작업 영역, 선택한 계정의 상세 inspector로 구성한다. 주요 작업은 한 화면 안에서 끝내고 중첩 modal과 불필요한 페이지 이동을 줄인다.
- **소재와 색:** 시스템 light/dark에 맞는 중립 surface와 절제된 accent를 사용한다. 지원되는 native material·반투명 효과는 sidebar/toolbar 등 chrome에 제한하고, 잔여량·계정·오류를 읽는 본문은 안정적인 대비를 유지한다. OS 버전별 효과 지원과 투명도 줄이기를 존중하며 유리 효과를 위해 가독성이나 성능을 희생하지 않는다.
- **글꼴과 밀도:** macOS 시스템 글꼴, tabular numerals, 명확한 크기·굵기 위계. 계정 정보는 왼쪽, 비교 숫자는 오른쪽 정렬한다. 공급자별 quota와 리셋 타임라인의 정렬을 시각적 특징으로 삼고 거대한 통합 수치 카드·장식성 gradient·동일 카드 격자는 피한다.
- **스위칭 UX:** 다음 실행 기본값과 현재 실제 계정을 공간적으로 분리한다. 클릭한 계정, 전환 진행, 검증 완료, 실패를 연속적으로 보여주며 오류는 선택한 행 가까이에 복구 동작과 함께 표시한다. hover에만 핵심 동작을 숨기거나 토스트만으로 적용 여부를 알리지 않는다.
- **상호작용:** 명확한 선택 상태, focus, 키보드 탐색, 적절한 문맥 메뉴와 Mac 단축키를 제공한다. motion은 전환·펼침·상태 변화의 인과관계를 설명할 때만 사용하고 reduced motion을 준수한다. 색과 animation 없이도 상태를 이해할 수 있어야 한다.
- **화면 검증:** 1100×720 기준으로 설계하고 900×600에서도 핵심 계정·상태·전환 버튼이 잘리지 않아야 한다. 실제 macOS 앱에서 light/dark, 계정이 많거나 없는 상태, 긴 계정명, quota unknown, 인증 만료, 전환 실패를 확인한다. 정적인 정상 화면만 예쁘게 만든 상태는 디자인 완료가 아니다.
- **Windows 후속:** 정보 구조와 도메인 동작은 재사용하되 창 chrome·단축키·시스템 소재는 Windows 관례에 맞춘다. Mac 외형을 그대로 복제하지 않는다.

## 6. 도메인 모델

| Entity | 핵심 필드 | 불변 조건 |
|---|---|---|
| Account | id, provider, subjectRef, workspaceRef, plan, enabled, policyScope | email 단독으로 내부 identity를 결정하지 않음 |
| CredentialBinding | id, accountId, adapter, adapterIdentityKey, profileRef, authOwner, cliVersion, verifiedIdentity, verifiedAt | credential bytes 없음; 관리자 경유 로그인 변경은 active session과 직렬화. native 내부 변경은 drift 감지 대상 |
| EntitlementPool | id, provider, upstreamPoolRef, scope, sharingEvidence, confidence, mappingRevision | 불확실한 pool도 독립적인 임시 ID로 보존; quota를 임의 병합하지 않음 |
| AccountPoolMembership | accountId, poolId, meterId | 하나의 계정이 공통·모델별 여러 pool에 속할 수 있음 |
| QuotaSnapshot | poolId, meterId, unit, used, limit, usedFraction, windowId, resetsAt, upstreamObservedAt, receivedAt, source, confidence, revision | source별 timestamp 유지; percent/token/USD 상호 임의 변환 금지 |
| ModelCapability | provider, exactModelId, family, requiredPools, authPaths, eligibilityEvidence | alias 해석 결과와 capability 검증 버전 기록 |
| Session | id, clientInstanceId, tool, cwdScope, profileBindingId, pidIdentity, actualIdentity, mode, state | desired identity와 actual identity 구분 |
| Lease | id, sessionId, generation, selectedBindings, reservations[], admissionState, heartbeatAt, processIdentity | provider/meter별 reservation; OS 프로세스 생존 확인 없이 재할당 금지 |
| Reservation | poolId, meterId, estimateUnit, expectedFuture, unsettledDebt, confidence, accountingMode | 같은 shared pool 예약 중복 차감 방지; debt는 같은 단위의 실측 소비에만 허용 |
| SwitchOperation | id, requestId, scope, sourceSessionId, targetBindingId, state, evidence, failureCode | VERIFIED 이전 성공 UI 금지 |
| Decision | id, policyRevision, snapshotRevisions, candidates[], exclusions[], selected, scores | secret/prompt/전체 argv 기록 금지 |

전체 token ledger와 quota ledger는 분리한다. 세션 token 수를 구독 percentage로 환산할 수 없다면 추정 신뢰도와 추정 불가를 기록한다.

`adapterIdentityKey`는 adapter가 요구하는 opaque key다. OMP의 `email:...|org:...` 형식도 그대로 보존하되 내부 Account ID를 email에서 생성하지 않는다. account.verify가 이 key와 provider subject/workspace의 대응 근거를 기록한다.

공유 관계 미확정 시 계정별 provisional pool snapshot을 그대로 유지하고, 별도 `AdmissionGroup`으로 동시성만 제한한다. 서로 다른 quota 수치를 합치거나 작은 값을 하나의 실측 pool처럼 표시하지 않는다. 공유 관계 확인 후 canonical mapping을 변경할 때는 해당 그룹 신규 admission을 멈추고 활성 lease·미정산 소비를 drain한 뒤 mappingRevision을 원자적으로 올린다. 과거 snapshot/decision은 원래 revision으로 유지한다. live reservation을 추정 병합·분할하지 않는다.

## 7. 배정 정책

새 세션의 계정 선택 방식은 두 가지다. 프로젝트 규칙을 먼저 만들 필요는 없다.

- **스마트 자동 배분(기본값)**: 요청한 도구·모델에 맞는 적격 계정에서 플랜별 공정 순서, 관측 잔여량과 리셋 시각, 단기 한도 압력, 동시 슬롯을 고려한다. 저장된 소비 순서는 무시한다. 균등 토큰 분배나 정확한 소비 예측을 뜻하지 않는다.
- **우선순위대로 사용**: 하드 필터를 통과한 계정 중 저장된 순서가 가장 앞선 계정을 선택한다. 소진·안전 여유량·동시 슬롯·인증·프로젝트 조건에 걸리면 다음 적격 계정을 사용한다. 지정 순서는 플랜별 스마트 순위보다 먼저 적용하며, 지정 계정이 모두 불가하면 순위 미지정 후보에 스마트 기준을 적용한다.

`Policy.allocationMode`는 `smart|priority`, `accountPriority`는 도구별 연결 계정 ID의 전역 순서다. 기존 저장 정책의 누락 필드는 `smart`와 빈 목록으로 읽는다. 중복·알 수 없는 계정 ID와 잘못된 모드는 CAS 갱신에서 원자적으로 거부한다. 안전한 등록 대체 시 순위 참조도 이전하고, 실제 삭제된 연결의 참조만 제거한다.

두 방식은 새 자동 배정에만 적용한다. 직접 지정·도구 기본 계정·프로젝트 pin·재개·부모 계정 고정은 계속 우선하며, 고정 계정이 불가하다고 우선순위의 다른 계정으로 바꾸지 않는다. 실행 중인 세션과 대화 재개는 원래 계정을 유지한다. 자동 배정 일시정지는 선택 방식과 별도 설정이다.

### 7.0 외부 대화 인계

외부에서 시작한 대화는 소유 계정을 실제 파일 근거로 확인한 뒤에만 관리로 인계한다. Claude는 대화 UUID가 계정 프로필의 `projects` 폴더에 있는지로, OMP는 대화 파일에 기록된 OAuth pin이 어느 연결과 일치하는지로 판단한다. 근거가 없거나 여러 계정에 걸리면 인계하지 않고 이유를 반환한다(`TAKEOVER_OWNER_UNKNOWN`, `TAKEOVER_UNSUPPORTED`).

인계는 실행 중인 작업을 끊거나 대화의 계정을 바꾸는 기능이 아니다. 확인된 소유 계정으로 등록해 두고, 다음 실행·재개부터 관리 세션(lease·동시 슬롯·프로젝트 규칙·종료 추적)으로 다룬다. 계정 배분은 새 세션에만 적용한다. 프로젝트 규칙이 다른 계정을 고정하더라도 인계 대화는 자기 계정을 유지하며, 부모 세션 계정과 다르면 자식 실행으로 재개하지 않는다.

`Policy.autoTakeover`(기본값 켜짐)가 켜져 있으면 외부 대화를 재개할 때 위 근거로 소유 계정을 확인해 자동 인계한다. 꺼져 있으면 사용자가 등록한 인계만 적용하고 나머지는 `RESUME_UNKNOWN`으로 남긴다. 저장된 인계도 매 요청에서 근거를 다시 확인하며, 관리 세션이 생기면 등록을 정리한다. RPC는 `takeover.adopt`/`takeover.release`, CLI는 `aam takeover list|adopt|release`다.

여러 계정·공급자를 함께 사용한 OMP 대화는 인계하지 않는다. 관리 실행본은 한 계정만 사용하므로 단일 계정으로 재개하면 다른 공급자 호출이 차단된다. 관측에서 대화 파일을 하나로 확정하지 못한 외부 세션도 인계 대상으로 제시하지 않는다.

### 7.1 하드 필터

1. 허용된 프로젝트/조직/공급자/모델/auth 경로.
2. 활성 계정·정상 인증·검증된 profile binding.
3. model capability와 필요한 모든 quota pool 확보.
4. 알려진 exhausted/cooldown/결제 차단 제외. 401/403을 무조건 다른 계정으로 돌리지 말고 원인을 구분.
5. 미확정 sharing group은 같은 그룹으로 동시 admission을 보수적으로 제한할 수 있지만, 확인된 pool 병합처럼 표시하지 않는다.

모든 후보가 탈락하면 이유와 복구 방법을 반환한다. 몰래 API 과금이나 다른 모델로 전환하지 않는다.

모델을 지정하지 않은 실행은 공식 CLI가 모델을 결정한다. 이때 모델 전용 한도는 입장 판정에서 제외하고 공통 한도만 검사한다. 소진된 모델 전용 한도는 `MODEL_LIMIT_GUARD`로 안내하며, 공식 설정(`<프로젝트>/.claude/settings.local.json` → `settings.json` → 계정 프로필 `settings.json`)에서 기본 모델을 확인했고 그 모델의 한도가 소진된 경우에는 `MODEL_LIMIT_EXPECTED`로 알리고 같은 조건의 다른 계정보다 뒤에 둔다. 실행 시점의 셸 환경 변수와 CLI 인수는 이 경로로 확인할 수 없으므로 실제 모델이 다를 수 있다.

관리 배정이 불가능하다는 이유로 사용자의 실행을 막지 않는다. 관리 shim은 배정·인증·사용량·동시 슬롯·서비스 연결 실패(`NO_ELIGIBLE_ACCOUNT`, `QUOTA_EXHAUSTED`, `SAFETY_RESERVE`, `CAPACITY_RESERVED`, `AUTH_REQUIRED`, `AUTOMATIC_PAUSED`, `DAEMON_UNAVAILABLE` 등)에서 이유를 stderr로 알리고 원본 CLI를 그대로 실행한다. 이 실행은 관리 세션으로 기록하지 않으며 계정은 공식 CLI가 결정한다. 사용자가 명시한 제한(프로젝트 허용 목록, 경로 규칙 충돌, 계정 지정 충돌, 대화·부모 계정 보호)은 통과 대상이 아니며 그대로 중단한다.

### 7.2 순위와 reservation

아래의 잔여량 기반 비교는 스마트 방식과 순위 미지정 후보의 fallback에 적용한다. 현재 구현은 관측 잔여량·안전 여유량과 실제 동시 슬롯을 사용하며, 소비 예측이나 토큰 예약을 만들어내지 않는다.

- 같은 provider·동급 plan/capacity·같은 작업 모델의 후보끼리 비교한다.
- 한 bucket의 `effectiveRemaining = observedRemaining - expectedFutureReservations - unsettledDebt - safetyReserve`.
- 과거 소비와 앞으로의 예약을 같은 시점의 수치처럼 중복 차감하지 않도록 아래 정산 규칙을 따른다.
- 이용 가능한 후보에 대해서만 `requiredDrain = max(effectiveRemaining,0) / hoursUntilReset`를 계산한다.
- reset이 이미 지났거나 unknown/stale이면 무한 urgency를 주지 않는다. 재조회 전 해당 시계 기반 순위 제외; 명시적인 manual launch 외 자동 선택에서 보류한다.
- Fable 등의 모델별 주간 bucket과 공통 bucket을 모두 admission 검사한다. 순위에 사용할 주간 bucket은 동일 단위로 보정 가능한 병목 처리 가능량 기준으로 선택; 보정 불가하면 model-scoped bucket을 우선하고 shared bucket은 hard guard로 둔다.
- 서로 다른 plan의 %는 실제 처리량이 아니다. 초기에는 plan class별로 비교하고 사용자 우선순위를 적용한다. 관측된 작업당 소비 추정이 충분해지면 동급 작업 단위로 보정한다.
- 건강한 기존 세션은 재배정하지 않는다. 새 작업만 urgency 우선. 동점은 이미 예약된 예상 부하가 적은 후보, 마지막 배정이 오래된 후보, 안정적인 account ID 순으로 결정한다.
- 같은 계정 여러 세션 허용 여부는 native refresh 동시성 검증과 명시적 concurrency limit에 달림. 검증 전 binding 동시 한도는 1이며, 같은 계정의 여러 터미널 사용을 막는 이 제한을 출시 기본값으로 굳히지 않는다. adapter의 독립 login/공유 profile 지원 방식에 따라 동시성 gate를 통과시킨 뒤 검증된 상한으로 높인다. 임의 token 복제는 금지. 서로 다른 binding이라도 같은 quota pool 예약은 합쳐 검사한다.
- 세션 길이는 무한할 수 있으므로 시작 시 전체 소모를 예약했다고 주장하지 않는다. 작업 유형별 예상 소비와 진행 관측으로 갱신하는 입장 제어다. 모델 요청 가로채기 없이 hard quota SLA를 제공하지 않는다.

### 7.3 관측·정산

- 우선순위: 공식 CLI 구조화 관측/공식 API > 검증된 기존 OMP 사용량 > 명시적 opt-in의 비공식 quota adapter > 로컬 추정. 원문 token/cookie를 UI에 전달하지 않는다.
- provider 상태 조회는 inference probe와 다르다. 자동 background 조회가 모델 답변을 생성하거나 크레딧을 소비하지 않아야 한다.
- 기본 polling 5분±25% jitter. 실제 HTTP cache/upstream timestamp 유지. single-flight는 account가 아니라 실 pool/source 범위까지 고려한다.
- 사용량 API가 없는 모델 bucket은 화면에 unknown. 다른 bucket의 값을 복사하지 않는다.
- accountingMode를 `metered`와 `snapshot-estimated`로 분리한다. 관측할 수 없는 세션 소비를 실제 debt로 만들지 않는다.
- `metered`는 quota와 같은 단위의 실제 요청 소비 및 snapshot 포함 경계를 검증할 수 있는 adapter에만 허용한다. event ID로 소비를 한 번만 기록하고, snapshot의 effective-as-of/revision이 포함을 증명한 소비만 debt에서 뺀다. 이 근거가 없는 native 구독 CLI는 기본적으로 `snapshot-estimated`다.
- `snapshot-estimated`는 pool의 새 authoritative snapshot을 전체 사용량 기준으로 삼는다. `unsettledDebt=0`은 소비가 없다는 의미가 아니라 attribution을 하지 않는다는 의미이며 UI에 이를 표시한다. 이미 발생했을 것으로 추정한 소비를 매 heartbeat마다 debt로 누적하지 않는다.
- 이 모드의 `expectedFuture`는 앞으로 일정 관측 horizon 동안의 소비 예측이다. 새 snapshot마다 현재 active session 집합의 forecast를 **교체 계산**하며 과거 forecast와 더하지 않는다. 잔여 예측량을 실제 소모/환급 ledger로 표시하지 않는다. 추정률이 없으면 forecast는 unknown으로 남기고, 신선한 quota·exhaustion·고정 safety reserve·binding/AdmissionGroup의 동시 슬롯으로 배정한다. quota 예약 SLA는 제공하지 않는다.
- source lag를 보정하는 allowance는 pool당 한 번의 safetyReserve로 적용하고 세션별 무한 debt로 쌓지 않는다. 문서화되거나 검증된 lag bound가 있으면 source 설정에 기록한다. bound가 없으면 `best-effort` 신뢰도를 표시하며 known exhaustion과 stale는 계속 차단한다. 시간 경과만으로 관측 신뢰도를 올리지 않는다.
- pool snapshot 감소분은 전체 pool에 이미 반영된 소비다. 여러 세션의 debt를 줄이기 위해 각각 재사용하지 않으며 세션별 attribution은 unknown으로 남긴다. 외부 사용도 같은 pool snapshot에 포함된다.
- `metered`의 source가 포함 경계 제공을 중단하면 새로운 정확 정산을 중지하고 해당 meter 신규 admission에 오류를 낸다. 기존 실제 debt를 2회 poll 등 임의 조건으로 지우거나 조용히 추정 모드로 전환하지 않는다. 지원 adapter 복구 또는 명시적인 위험 고지 후 모드 변경 절차가 필요하다.
- reset 확인 시 window generation을 바꾼다. 과거 window의 실제 debt를 새 quota에 차감하지 않는다. reset 경계를 넘는 active 작업의 미래 forecast는 새 generation에 다시 계산한다.
- 새 snapshot과 lease commit은 revision 검사를 통해 일관되게 처리한다. 외부 네트워크 조회는 DB transaction 밖에서 수행한다.

### 7.4 기본값의 성격

5분 poll, 10초 heartbeat, 30초 prepared timeout, 45초 suspect threshold는 검증 가능한 초기 운영값이지 provider 보장값이 아니다. 설정으로 조정 가능하며 로그로 빈번한 경보·쿼리 실패를 측정해 보정한다. suspect threshold는 quota를 즉시 반환하는 TTL이 아니다.

## 8. 동시 실행과 프로세스 수명

### 8.1 원자적 배정

`lease.acquire(requestId, clientInstanceId, launchIntent, policyRevision)`:

1. 필요한 관측이 stale이면 transaction 밖에서 bounded refresh. quota source 오류를 신선한 snapshot으로 기록하지 않음.
2. SQLite `BEGIN IMMEDIATE` 안에서 requestId 재시도 확인, 최신 snapshot/policy, 활성·불확실 lease, 모든 pool 예약을 읽는다.
3. 후보 선택과 multi-pool reservation, decision, PREPARED lease를 원자적으로 저장. 전부 성공하거나 전부 실패.
4. commit 후 opaque lease capability와 검증된 launch plan reference를 반환한다. 응답 유실 뒤 같은 requestId+payload 재시도는 같은 lease의 **현재 상태**를 반환하며 다른 payload는 conflict. ABORTED/FAILED 상태는 spawn 권한이 아니고 새 요청 ID로 재획득해야 한다.
5. launcher는 `lease.starting(capability, generation, spawnAttemptId, supervisorIdentity)`를 호출한다. daemon은 PREPARED 유효성·현재 policy/profile revision을 CAS 검사하고 STARTING을 내구성 있게 저장한 뒤 단 하나의 spawn attempt에 ack한다. ack 전에 exec 금지.
6. timeout 정리와 starting 전이는 같은 writer/transaction으로 직렬화한다. PREPARED timeout이 먼저 승리하면 starting은 거부된다. STARTING이 먼저면 timeout으로 환급할 수 없다. supervisor는 spawnAttemptId를 한 번만 실행하고 같은 응답을 다시 받아도 재실행하지 않는다. supervisor 재시작 시 미확정 attempt를 재사용하지 않는다.
7. 명백한 OS spawn 실패만 `lease.abort`로 FAILED 환급한다. ack 유실·spawn 결과 불명·PID 보고 유실은 SUSPECT로 남기고 살아 있는 child를 조사한다. generation이 바뀐 이전 요청은 거부하지만 이미 ack한 STARTING을 daemon 재시작만으로 회수하지 않는다.

### 8.2 상태 전이

`PREPARED → STARTING → ACTIVE → DRAINING → EXITED → RECONCILED`

예외: `PREPARED → ABORTED`, `STARTING → FAILED(명백한 미실행 증거)/SUSPECT`, `ACTIVE/DRAINING → SUSPECT`, `SUSPECT → ACTIVE/ORPHANED/EXITED`.

- process identity는 PID 단독이 아니라 boot ID + PID + OS 시작 시각 + 실행 파일 identity다.
- STARTING은 PID 기록 전 crash 구간을 포함한다. daemon의 starting/started transaction log가 journal이며 launcher는 DB를 쓰지 않는다. starting에 기록한 supervisorIdentity와 spawnAttemptId로 recovery를 연계한다. source 확인 없이 orphan을 다른 lease에 붙이지 않으며 모호한 시작 결과를 자동 재실행하지 않는다.
- heartbeat 중단은 SUSPECT 전환만 한다. 프로세스 생존 여부가 불명확하면 reservation과 concurrent slot을 유지한다.
- daemon 재시작은 persisted lease부터 복구한다. 기존 CLI를 중단하지 않고 admission을 reconciliation 완료 후 재개한다.
- sleep/wake 시 lease TTL만으로 슬롯을 반환하지 않는다. 로컬 OS 프로세스 재확인과 관측 갱신 후 배정 재개. monotonic elapsed와 UTC quota 시간을 구분한다.
- wrapper 종료 후 child가 살아 있으면 ORPHANED로 유지. 사용자 확인 없이 kill하지 않는다.
- suspend된 프로세스는 idle이 아니라 계속 계정을 소유하는 세션이다. 단기 예약 최적화와 binding 소유권을 분리한다.
- native CLI background 작업·내부 subagent가 foreground 종료 후 계속 실행될 수 있다. adapter가 descendant/backend task 수명을 관측할 수 없으면 종료 즉시 완전 환급하지 않으며 추정 상태를 표시한다.
- wrapper는 TTY를 가짜 pipe로 바꾸지 않는다. TUI stdin/out/err, resize, signal, job control, raw exit code를 보존하고 JSON print-mode stdout에 관리 로그를 섞지 않는다.
- provider 네트워크가 끊기면 기존 작업은 native 동작에 맡긴다. 관리자는 새 계정 이동으로 기존 side effect를 재생하지 않는다.

### 8.3 중첩 실행

- 부모 native subagent는 별도 프로세스 식별이 가능하지 않으면 부모 lease의 fan-out 예상 부하에 포함한다. 독립 추적했다고 표시하지 않는다.
- 관리형 shim을 통해 새 CLI를 호출한 child는 parentSessionId와 독립 requestId로 신규 lease를 받는다. 부모 lease capability를 재사용해 다른 모델·공급자 권한을 넓히지 않는다.
- 다른 provider를 사용하는 OMP subagent는 아래 OMP launch scope에 반드시 포함돼야 한다. 포괄적인 무제한 pool을 남기지 않는다.

## 9. Adapter 계약과 스위칭

### 9.1 공통 adapter 인터페이스

`discoverBinary`, `inspectCapabilities`, `enrollProfile`, `inspectIdentity`, `collectQuota`, `resolveLaunchScope`, `buildLaunchPlan`, `verifyStartedIdentity`, `observeLifecycle`, `prepareSwitch`.

각 adapter는 `profileIsolation`, `quotaRead`, `identityRead`, `resumeSameAccount`, `resumeCrossAccount`, `inProcessSwitch`, `modelScopeControl`, `backgroundTaskTracking`을 verified/unsupported/unverified로 반환한다. 기능이 문서에 존재하는 것과 현재 설치 버전에서 검증된 것은 별도 필드다.

### 9.2 새 세션 시작

1. 계정 등록은 native login으로 진행. 암호/OTP는 앱에서 수집하지 않는다. 사용자 브라우저 인증으로 인계한다.
2. 계정 profile과 실제 native identity를 비교해 binding을 생성한다. identity를 읽지 못하면 표시만 등록할 수 있으나 verified 자동 배정 대상이 아니다.
3. launch intent는 모델, 도구, project scope, 인터랙티브/배치, 허용 provider 집합을 포함한다. 명시한 모델은 그대로 사용한다. 모델을 생략하면 `native-default`로 배정 자격을 검사하고 `--model`을 주입하지 않아 공식 CLI의 기존 설정을 유지한다. 실제 family를 추측하지 않으며 기본 설정에서는 확인된 모델별 quota를 모두 보수적으로 검사한다.
4. 인증 우선순위에 영향을 주는 env/CLI flag/사용자·프로젝트·managed 설정을 preflight한다. API key, base URL, apiKeyHelper, cloud provider가 profile을 덮으면 `AUTH_OVERRIDE_CONFLICT`. 값을 출력하거나 사용자 설정을 임의로 지우지 않는다.
5. 선택·예약 후 동일한 최종 env/프로필/실행 파일/해석된 auth 설정으로 native identity probe를 실행한다. 대상 identity와 일치하고 auth override가 없다는 `preflight-verified` evidence를 얻기 전에는 prompt를 native CLI에 넘기지 않는다. 관리자가 소유한 auth/config 변경은 이 구간 동안 직렬화하며 외부 편집은 감지 가능한 범위에서 revision 재검사한다.
6. §8의 starting ack 뒤 native binary를 구조화 argv로 실행한다. preflight evidence + spawn 성공으로 ACTIVE가 될 수 있지만 UI는 `시작 전 계정 확인` 등 검증 수준을 표시한다. profile 경로만 확인한 `configured`는 이 수준이 아니다.
7. 실행 중 identity를 native API/event로 확인할 수 있으면 `runtime-confirmed`, 실제 upstream evidence가 있으면 `upstream-confirmed`로 올린다. 사후 mismatch가 발견되면 첫 요청이 이미 전달·소비됐을 수 있음을 기록하고 새 작업 재생 없이 중단/사용자 복구로 연결한다. TTY/print-mode를 가로채지 않는 launcher가 모든 첫 요청의 upstream 계정을 무오류 보증한다고 주장하지 않는다.

**기존 사용자 환경 보존:** profile마다 permissions·sandbox·MCP·skills·plugins·모델 설정을 기본값으로 잃어버리지 않도록, 인증이 없는 설정만 allowlist 기반으로 연결/복사하고 사전 diff를 보여준다. credential·세션 전체 디렉터리를 symlink하거나 secret 포함 설정을 무작정 복제하지 않는다. native managed policy는 항상 유지한다. auth override가 섞여 있으면 복제하지 않고 충돌로 표시한다.

**Resume:** `(tool, nativeSessionId) → bindingId` 매핑을 저장하고 `--resume`/continue는 원래 binding에 고정한다. 자동 배정이나 새 기본 계정 선택으로 기존 대화를 다른 계정에서 재개하지 않는다. 이전에 관리하지 않던 세션은 native metadata로 원래 binding을 확인한 뒤 연결하며 확인 불가면 사용자가 프로필을 명시하도록 안내한다. 계정 간 resume은 §9.4의 별도 지원 gate를 거친다.

**수동 선택 우선순위:** 명시적 target binding은 해당 launch intent의 후보를 그 binding으로 제한한다. 자격·quota·정책 검사를 통과하지 못하면 다른 계정을 대신 선택하지 않는다. 기존 세션 resume의 binding과 수동 target이 다르면 일반 실행이 아니라 cross-account switch 요청으로 분리한다.

### 9.3 도구별 연결

**Claude:** 계정마다 CLAUDE_CONFIG_DIR. Keychain namespace 분리가 공식 문서에 명시돼 있다. 동일 env의 auth status JSON을 시작 전 gate로 사용한다. 실행 이후 identity metadata가 있으면 검증 수준을 높이지만 statusline quota만으로 identity를 증명하지 않는다. batch/`-p`에 statusline이 없다고 자동 배정을 불가능하게 하지 않으며 preflight-verified 수준으로 표시한다. statusline quota helper는 metadata만 전달하고 기존 사용자 statusline 출력은 보존. `ANTHROPIC_API_KEY`와 기타 auth override를 감지하며 사용자·조직 policy를 덮어쓰지 않는다.

**Codex:** 계정별 CODEX_HOME. 선택한 credential store의 namespace 격리를 검증한다. UI headless 제어가 필요한 경우 공식 app-server account/read 및 rateLimits 사용. native TUI를 app-server 프로세스와 임의로 합성하지 않으며 설치 버전 지원 transport만 사용. refresh token을 복제해 여러 저장소의 갱신 경쟁을 만들지 않는다.

**Grok:** GROK_HOME profile. leader socket이 전역 기본 경로를 쓰면 해당 CLI 지원 flag로 profile별로 분리하고 실제 routing을 검증한다. quota 소진 상태는 별도 auth failure로 오분류하지 않는다. CodexBar billing RPC가 일부 버전에서 미지원임을 문서화하므로 공식 CLI 경로가 확인되지 않은 endpoint는 `experimental, opt-in`이며 silent fallback 금지.

**OMP:**
- 원본 `omp` 바이너리와 전역 설정은 변경하지 않는다. 앱에 포함한 별도 `aam-omp`만 관리하며, 공식 OMP 18.2.6과 Bun 1.4.2의 버전·패키지 integrity를 고정한다. 설치된 AAM shim으로 해석되는 새 `omp` 명령에는 공통 프로젝트 정책이 적용된다. 기존 실행 중인 원본 OMP는 재시작하거나 편입하지 않는다.
- UI에서는 OAuth 계정과 `provider/model`을 명시한다. `aam run omp`의 자동 배정은 시작 시 한 번만 선택하며, 선택 이후에는 A 하나로 고정한다. A의 소진·차단·삭제·비활성화·중복 식별 또는 인증 경로 위반 시 `AAM_OMP_ACCOUNT_LOCKED`/78로 중단하고 B/C나 API 키로 재시도하지 않는다.
- 관리 SDK의 AuthStorage 선택·갱신·차단, 모델 registry, stream 진입점과 worker 정책 전달에서 같은 제한을 적용한다. 환경 API 키는 존재만으로 오류 처리하지 않지만 인증 후보로 사용하지 않는다. runtime/config/fallback 키, broker, provider 등록과 인증·전송 경로 override는 허용하지 않는다.
- main·subagent·보조 호출도 선택한 공급자와 공식 모델 경로에 한정한다. 다른 공급자를 요구하면 모델이나 계정을 자동 변경하지 않고 차단한다. 여러 공급자를 한 managed session에서 묶는 기능은 제공하지 않는다. 저장된 관리 대화는 같은 계정으로 재개하며 관리 OMP fork는 지원하지 않는다.
- 계정 목록·실행 전 검사는 공식 저장소의 schema 7을 readonly로 열어 비밀 없는 identity metadata만 반환한다. 인증 DB나 token을 복제·내보내기 하지 않는다. 실행 시에는 기존 DB만 열고, 선택한 A의 공식 refresh/lease/CAS를 허용한다. 다른 계정의 정리·중복 병합·schema migration은 실행하지 않으며 연결별 revision 추적은 TEMP 객체만 생성한다.
- 같은 OAuth identity의 native Claude 연결과 관리 OMP 연결은 슬롯을 공유하고 연결별 동시 실행 상한 중 작은 값을 적용한다. 기존 외부 OMP는 관측 대상으로만 남으며 관리 슬롯에 소급 편입하지 않는다.
- 보장 범위는 앱 소유 실행본의 정상 OMP 인증·SDK 경로다. 신뢰하는 일반 확장과 기존 관측 확장은 유지하지만, 악성 확장·임의 JavaScript·직접 HTTP·외부 CLI·같은 OS 사용자 권한까지 격리하는 sandbox는 아니다. 원본 OMP 전체나 모든 upstream 요청의 독립적인 identity 증명으로 표시하지 않는다.

**Google:**
- agy와 legacy Gemini CLI를 서로 다른 adapter로 등록한다. 개인 agy 구독을 API key 과금으로 자동 대체하지 않는다.
- G-GOOGLE에서 per-process auth·keyring·backend·session 격리가 입증돼야 자동 전환을 활성화한다. 현재 확인된 자료만으로는 통과하지 못했다.
- 전역 logout/login/symlink 교체를 동시 실행 계정 전환으로 사용하지 않는다. 실제 지원 수단이 없으면 공급자 upstream 기능 추가 또는 사용자 승인된 별도 격리 환경이 필요하다. 다른 OS 계정/VM을 자동으로 설치하지 않는다.
- 단일 계정 관측·실행과 여러 Google 계정 동시 전환은 별도 capability다. 후자 미지원 상태를 전자 성공으로 가리지 않는다.

### 9.4 수동 전환의 정확한 의미

UI 동작을 세 가지로 분리한다.

1. **새 세션 기본 계정 변경**: policyRevision을 갱신한다. 현재 세션은 그대로. UI에 `다음 실행부터 B · 현재 A 세션 2개 유지`라고 표시.
2. **B 계정으로 새 세션 열기**: B를 예약하고 identity 확인 후 새 세션 성공. 원래 A 세션의 대화/작업이 옮겨졌다고 말하지 않는다.
3. **현재 작업을 B로 이어가기**: native adapter가 안전한 작업 경계와 계정 간 resume을 실제 지원할 때만 제공. 미지원이면 명시적인 인계 문서를 사용한 새 세션 시작으로 안내하고 사용자 확인 없이 대화 기록·파일·조직 데이터를 복사하지 않는다.

새 세션 전환: `REQUESTED → VALIDATING → RESERVED → LAUNCHING → VERIFYING → VERIFIED`, 예외 `FAILED/CANCELLED`. 실제 native 실행 직전 §9.2의 preflight identity gate를 필수로 통과한다.

현재 작업 인계: `REQUESTED → VALIDATING → WAITING_BOUNDARY → RESERVED → SOURCE_STOPPED → SNAPSHOT → LAUNCHING → VERIFYING → VERIFIED`. WAITING_BOUNDARY에서는 타깃을 추천 후보로만 보유하고 hard quota/동시 슬롯을 예약하지 않는다. 기본 10분의 취소 가능 대기 deadline 이후 자동 CANCELLED; 원래 세션은 유지한다. 안전한 경계가 확인된 뒤 타깃을 재검증·예약하며 예약 실패 시 source를 종료하지 않는다. SOURCE_STOPPED 이후 예약은 일반 PREPARED/STARTING fencing과 같은 수명 규칙을 따른다.

- operation별 idempotency key. 두 창에서 동시에 같은 세션 전환 시 revision conflict로 한 작업만 진행.
- 새 세션 시작 실패는 원래 세션을 그대로 보존한다. 작업 인계는 준비와 실행을 분리한다. 타깃 사전 준비·예약 뒤 사용자가 동의한 경계에서 source 종료를 확인하고서만 transcript snapshot/명시적 인계 자료를 만들어 target에 전달한다. source와 target이 같은 작업을 동시에 실행하지 않는다.
- source 종료 이후 실패의 보존 보장은 살아 있는 source 프로세스가 아니라 **원래 계정의 transcript 무손상과 같은 계정 resume 경로 제공**이다. source를 자동 재실행하지 않는다.
- 사전 identity probe 실패면 native prompt 전달 없이 중단한다. 사후 mismatch면 이미 전달된 요청 가능성을 고지하고 새 요청을 중단한다. 이미 실행된 외부 side effect를 없었던 것으로 간주하지 않는다.
- 세션 경계/지원되는 resume을 관측할 수 없으면 작업 인계 버튼을 비활성화하고 별도 새 세션 경로를 제공한다. 구현하지 못한 hot-swap을 WAITING 상태로 숨기지 않는다.
- VERIFIED는 최종 preflight-verified evidence와 정상 시작의 조합을 최소 수준으로 한다. runtime-confirmed/upstream-confirmed는 별도 badge다. 프로세스 생성/환경변수 설정만 성공한 configured는 전환 완료가 아니다.
- lease switch와 native `/login`, `/logout`은 구분한다. 관리자 경유 로그인 변경은 active 세션 drain/충돌 검사를 먼저 수행한다. CLI 내부 변경까지 가로챈다고 약속하지 않으며 감지한 경우 identity drift를 기록하고 그 binding 신규 admission을 차단한다.

### 9.5 호스트 공통 실행 계약

목표는 앱에서 별도 Terminal을 여는 경로에 한정하지 않고, 기존 호스트에서 OMP·Claude Code를 평소처럼 실행하는 흐름을 연결하는 것이다. 호스트별 계정 엔진을 중복 구현하지 않으며 공통 정책·lease·도구별 인증 adapter에 얇은 실행 연결을 둔다.

- 지원 단위는 `호스트 × 도구 × 실행 방식`이다. Orca TUI, Superset terminal command, cmux terminal, Conductor의 Claude executable override, VS Code/Cursor terminal과 Claude 확장 wrapper를 구분한다. 문서에 설정이 존재하는 것, 실제 설정됨, 실제 실행 검증됨을 혼용하지 않는다. Conductor의 OMP 내장 harness와 원격 엔진 미설치 호스트는 지원했다고 표시하지 않는다.
- `Policy.projectRoutes`의 각 항목은 `path`, `scope(directory|repository)`, `tool`, `mode(pinned|automatic|unmanaged)`, nullable `accountId`·`model`을 갖는다. directory는 가장 깊은 canonical 경로를 우선하고 repository는 Git common directory로 연결된 worktree까지 식별한다. 경로 접두 문자열만으로 다른 프로젝트를 같은 범위로 취급하지 않는다. 기존 도구별 기본 계정·자동 배정은 일치하는 세부 규칙이 없을 때 적용한다. 관련 없는 삭제된 폴더 규칙은 다른 작업을 막지 않지만, 저장소 identity를 확인할 수 없는 규칙은 해당 경로를 알리는 오류로 중단한다.
- `route.resolve`는 기존 launch intent로 적용 정책을 조회한다. 실제 `lease.acquire`와 `route.explain`도 같은 정책을 적용하며 클라이언트의 사전 조회만 신뢰하지 않는다. pinned 계정의 자격 미달 시 다른 계정을 고르지 않는다. automatic도 세션 시작에 한 번만 선택한다.
- 일반 작업의 원본 실행 통과는 명시적인 unmanaged 규칙에 한정한다. 서비스 장애·정책 해석 실패·알 수 없는 계정·인증 충돌은 원본 실행의 이유가 될 수 없다. host가 전달한 Claude profile과 AAM의 선택이 충돌하면 값을 지우고 덮어쓰지 않고 중단한다. 단독 `--help`·`--version`과 공식 Claude `auth status` 같은 기존 읽기 전용 정보 조회, 공식 자체 업데이트(`claude install|update|upgrade`, `codex update`)는 배정하지 않고 원본 CLI로 실행한다.
- resume/continue와 자식 실행은 원래 binding에 고정한다. 새 기본 계정 변경으로 원래 대화를 다른 계정에서 재개하지 않는다. 관리되지 않은 대화의 계정을 파일명으로 추측하거나 자동 가져오지 않는다. 일반 자식 CLI는 독립 lease·동시 실행 검사를 적용하며, 공식 자기 재실행은 검증된 기존 실행 문맥을 유지한다. 알려진 관리 대화는 unmanaged 폴더에서도 원래 계정을 유지한다. 정확한 UUID/OMP 파일의 매핑이 정말 없을 때만 별도의 명시적 unmanaged 규칙으로 native 실행을 허용하며, 모호한 ID·활성 세션·확인 실패는 원본으로 넘기지 않는다. continue는 가장 최근 기록이 활성/미확인이어도 이를 건너뛰어 더 오래된 대화를 고르지 않는다.
- OMP 관리 대화는 검증된 기본 프로필의 `sessions/aam-managed/*.jsonl` 아래에 새로 생성한다. 기존 외부 파일은 변경하지 않는다. 저장 경로와 native header UUID는 서로 다르므로 native의 제한된 metadata를 확인해 manager 매핑으로 해석한다. native `listAllSessions`의 탐색 구조를 유지하며 임의의 외부 transcript를 관리 대화로 가져오지 않는다. UUID 대소문자와 안전한 경로 별칭을 정규화하고, 기존 symlink의 대상 변경·사라진 파일·서로 다른 계정의 매핑 충돌은 확인 실패로 처리한다.
- argv·모델·TTY·표준 입출력·JSON protocol·종료 코드·signal을 보존한다. host가 넣은 새 대화 UUID와 인증 없는 hook 설정은 구조적으로 검증하며 인증·provider override로 우회할 수 없게 한다. fork는 resume-in-place로 바꾸지 않는다.
- 명시한 CLI 모델이 프로젝트 기본 모델보다 우선한다. 프로젝트 모델은 모델을 지정하지 않은 새 실행의 기본값이며, 지원하지 않는 공급자나 계정을 맞추려고 다른 모델로 대체하지 않는다. 관리 UUID와 native UUID는 구분하고 도구별 ID 충돌을 거부한다.
- 공식 등록으로 같은 프로필의 identity 없는 이전 항목을 안전하게 대체하는 경우에만 선호 계정·허용 목록·프로젝트 pin을 새 binding으로 함께 옮긴다. 일반 계정 삭제나 근거 없는 관측 병합에서는 pin을 다른 계정으로 바꾸지 않으며, 남은 규칙은 실행을 차단한다.
- Claude의 `CLAUDE_CODE_PROCESS_WRAPPER`는 일반 supervisor-spawn과 다른 exec 계약이다. 원본 argv·환경을 유지하고 stdout을 오염시키지 않으며 중첩 호출·짧은 시작 제한·기존 실행의 계정 문맥을 검증한다. 기존 실행 중인 background service나 전역 설정을 재시작·변경해 적용 범위를 확대하지 않는다.
- 신규 경로의 완료 증거에는 두 CLI의 실제 host 시작·동일 계정 재개·종료·점유 반환, API/다른 계정 fallback 거부, 서비스 장애·프로필 충돌, 기존 프로세스 보존을 포함한다. in-process 로그인 변경과 같은 UID의 임의 코드 실행까지 차단하는 OS sandbox로 표시하지 않는다. 이전 OMP 응답 후 exit 78의 원인은 재현 증거 없이 해결됐다고 표시하지 않는다.

## 10. API와 오류 계약

아래 이름은 최초 설계의 제품 API 계약이며 기존 공식 CLI 명령이 아니다. 현재 구현된 표면과 미지원 범위는 17절에 구분한다.

| API | 입력 요지 | 결과 |
|---|---|---|
| `status.read` / `status.subscribe` | revision | 계정·pool·세션·서비스 상태의 token-free snapshot/delta |
| `account.enroll` | provider, adapter, profileName | native login handoff와 operation ID |
| `account.verify` | bindingId | identity evidence, capability gate 결과 |
| `quota.refresh` | pool IDs | 갱신 결과/실제 freshness, rate-limit 시 다음 허용 시각 |
| `route.explain` | launchIntent | 선택 후보·이유만. lease 생성 없음 |
| `lease.acquire` | requestId, clientInstanceId, intent | committed lease/generation/launchPlanRef |
| `lease.starting` | capability, generation, spawnAttemptId, supervisorIdentity, preflightEvidence | PREPARED→STARTING durable CAS ack. ack 전 spawn 금지 |
| `lease.started` | capability, spawnAttemptId, processIdentity, verificationTier | ACTIVE 또는 mismatch. 같은 attempt 중복은 idempotent |
| `lease.heartbeat` | capability, sequence, lifecycle metadata | 갱신 결과. token bytes 없음 |
| `lease.abort` / `lease.release` | capability, reason, execution evidence | 중복 호출 안전; 실측 debt와 추정 forecast는 accountingMode별 처리 |
| `policy.update` | expectedRevision, patch | CAS revision 또는 conflict |
| `switch.prepare` / `switch.commit` / `switch.cancel` | requestId, source, target, scope, expectedRevision | switch state + 적용 근거 |
| `diagnostics.export` | time range, redact=true | token/prompt/email 기본 제거된 진단 |

예상 오류: `DAEMON_UNAVAILABLE`, `PROTOCOL_MISMATCH`, `NO_ELIGIBLE_ACCOUNT`, `AUTH_REQUIRED`, `AUTH_OVERRIDE_CONFLICT`, `IDENTITY_MISMATCH`, `QUOTA_STALE`, `QUOTA_UNKNOWN`, `POOL_IDENTITY_UNVERIFIED`, `CAPACITY_RESERVED`, `ADAPTER_UNVERIFIED`, `SWITCH_UNSUPPORTED`, `SESSION_BUSY`, `POLICY_CONFLICT`.

오류는 code, 한국어 message, affected binding/pool의 내부 ID, retryable, nextAction을 반환한다. 계정 token이나 전체 provider 응답을 오류 문자열에 포함하지 않는다. 403은 정책/인증/결제/소진을 구분할 evidence가 없으면 unknown으로 남긴다.

## 11. 설치·업데이트·해제

### 11.1 macOS 우선 설치형 제품

- arm64 macOS 앱과 DMG를 제공. 일반 사용자는 Rust/Node/Homebrew를 설치할 필요가 없어야 한다. provider CLI는 기존 설치를 연결하며 미설치일 때 공식 설치 안내를 제공한다.
- 앱 첫 실행: CLI 탐지 → capability 진단 → 계정 등록 → quota 확인 → launcher 연결 미리보기 → 사용자 승인 → dry-run → 실제 사용.
- shim은 앱 관리 전용 디렉터리에 설치하며 원래 `~/.local/bin/claude` 등을 덮어쓰지 않는다. 실제 바이너리 경로는 shim 생성 전 확보하고 self-recursion 검증. `integration.json`(version 2)과 계정 binding은 원본 CLI의 진입 경로(symlink 포함)를 저장하고 실행·검증할 때마다 대상을 해석한다. 공식 자체 업데이트가 진입 링크를 새 버전으로 바꾸면 재연결 없이 따라가며, 버전별 경로를 고정했던 판 없는 기록은 원본 탐색에 쓰지 않는다.
- PATH 변경은 정확한 diff와 적용 shell 범위를 보여주고 승인 후 수행. 기존 shell은 재시작/rehash 안내. GUI 앱에서 PATH가 다르면 실행 경로를 명시적으로 설정하도록 안내하며 Orca 설정을 몰래 수정하지 않는다.
- foreground는 `aam run <tool> -- <native args>`로도 사용 가능. `aam doctor`, `aam status`, `aam explain` 제공. CLI-only 제품으로 완료 처리하지 않는다.
- 개인 LaunchAgent는 서명된 설치 경로의 서비스 바이너리를 참조. 서비스가 독립 관리되므로 창을 닫아도 동시 배정과 세션 추적은 계속된다.
- 대시보드에서 `창 닫기`, `앱 종료(서비스 유지)`, `새 배정 일시정지`, `서비스 중지`를 구분. 서비스 중지는 활성 세션 영향과 관리 해제 상태를 확인받고 수행.
- Developer ID signing + notarization을 외부 배포 gate로 둔다. 서명 자격이 없으면 로컬 개발 빌드는 가능하나 Gatekeeper 정상 배포 완료라고 주장하지 않는다. SIP/Gatekeeper 전역 비활성화를 요구하지 않는다.
- App Store sandbox 배포는 범위 밖. 사용자 승인된 CLI 실행을 위한 최소 entitlement만 사용하고 Full Disk Access/Accessibility는 기본 요구하지 않는다.
- updater 배포 서명과 Apple code signing은 별개. 자동 업데이트는 실행 중 launcher/서비스를 깨뜨리지 않도록 drain 또는 호환 버전 병행 후 전환한다.
- DB schema 변경은 백업과 forward migration, 최소 지원 protocol 범위를 가진다. 구버전 서비스가 새 DB를 쓰면 실패시켜 손상을 방지. rollback 불가능 migration을 자동 되돌리지 않는다.
- 제거 시 shim/PATH managed block/LaunchAgent만 정리. provider 계정 logout·credential 삭제·세션 기록 삭제는 별도 명시적 동의. 사용자가 수정한 shell block은 diff를 보여주고 보존 우선.

### 11.2 Windows 후속 경계

UI·Rust domain·scheduler·DB schema·adapter policy는 공통. 플랫폼 서비스 설치/IPC/credential 참조/프로세스 트리/TTY/경로 탐지만 분리한다.

| 영역 | macOS 우선 | Windows 후속 |
|---|---|---|
| 설치 | .app + DMG | NSIS/MSI 중 배포 정책 확정 후 선택 |
| background | 사용자 LaunchAgent | 사용자 로그인 startup/예약 작업 등 사용자 단위 lifecycle |
| IPC | private UDS + peer UID | named pipe + 사용자 SID DACL |
| credential | native CLI Keychain/profile | native CLI Credential Manager/profile |
| process | PID start/boot identity, Unix job control | process creation time, Job Object/ConPTY 등 adapter 검증 |
| path | POSIX, shell PATH | Unicode/spaces, PATH/PATHEXT, PowerShell/CMD |

Windows의 후속 구현 전에는 Windows 지원 표기를 하지 않는다. macOS에서 실행한 테스트가 Windows 검증을 대체하지 않는다.

## 12. 보안·장애·정책

- Tauri WebView는 bundled local asset만 로드; remote content/scripts 비활성. 좁은 CSP, Rust allowlisted commands만 노출. frontend generic shell/filesystem plugin 권한 부여 금지.
- UI 입력 account ID를 shell 문자열로 보간하지 않는다. 실행 파일 경로·signature/version은 검증된 registry에서만 선택.
- provider credential export/import 기능은 초기 범위에 넣지 않는다. 기존 로그인은 profile 연결 또는 native 재로그인; token 복사 도구가 아니다.
- OMP broker 연동은 client-side pool이 보안 경계가 아님을 안내. 허용되지 않은 조직 데이터는 정책상 그 provider launch 대상에서 제외하며 native sandbox와 혼동하지 않는다.
- CLI/collector update로 schema가 바뀌면 parser를 관대한 0% fallback으로 만들지 않는다. capability validation 실패와 원문 제거된 진단을 반환한다.
- known quota endpoint 429는 즉시 재시도 폭풍을 만들지 않고 Retry-After/backoff 준수. 인증 실패를 반복 poll로 해결하려 하지 않는다.
- 서비스 down: 새 managed launch는 기본 실패와 복구 안내. 자동 native fallback 금지. 사용자가 `unmanaged 실행`을 명시하면 관리 보장 제외를 표시한다.
- 기존 세션은 서비스 장애로 kill하지 않는다. 복구 전 새로운 배정 금지; 복구 후 불확실 프로세스는 보수적으로 유지한다.
- provider 정책상 금지된 구독 토큰 중개는 지원하지 않는다. 미확인 정책은 `requires-policy-review`; UI 동의만으로 공급자 허가가 생긴다고 설명하지 않는다.
- native binary에 제공된 credential/동일 UID profile을 사용자가 바꾸는 행위까지 방지하는 보안 제품이 아님을 명시한다. 감지 가능한 drift는 실제 identity 재확인과 경고로 처리한다.

## 13. 구현 작업 분해와 의존성

아래는 최초 작업 분해다. 실제 구현은 `crates/{protocol,adapters,service,launcher}`와 `apps/desktop`에 있으며 scheduler·저장은 service crate 내부에 배치했다.

**T0 feasibility를 먼저 수행한다.** Google 격리, 각 native CLI preflight identity, 한 계정 다중 터미널, OMP effective scope를 작은 실행 실험으로 확인하고 capability matrix를 채운다. 로그인·live inference가 필요한 시점에는 사용자가 인증에 참여하고 소모 범위를 확인한다. 해당 기능이 불가능하면 UI부터 완성해 지원하는 것처럼 보이지 말고 대체안/범위 결정으로 돌아온다. 현재 문서가 T0 통과를 대신하지 않는다.

| 작업 | 소유 경계/계획 경로 | 완료 증거 |
|---|---|---|
| T1 Domain/IPC schema | `crates/core`, `crates/protocol` | snapshot·lease·switch의 serialization 및 불변 조건 |
| T2 macOS service/lease | `crates/service`, `crates/platform-macos` | 실제 여러 프로세스의 atomic admission·crash 복구 |
| T3 Native CLI adapters | `crates/adapters/{claude,codex,grok,google,omp}` | 계정 격리/실행 identity/환경 충돌/프로세스 수명 capability 기록 |
| T4 Launcher supervision | `crates/launcher` | native TUI·pipe·signal·resume·중첩 호출 동작 |
| T5 Desktop/tray | `apps/desktop` | 실제 Tauri 창에서 계정 표·session·switch 성공/실패 UI |
| T6 macOS packaging | `packaging/macos` | 앱 설치→CLI 연결→동시 세션→업데이트→제거 end-to-end |
| T7 Provider release gates | adapter별 evidence | 4개 공급자의 실제 계정 흐름, 미확인 기능 표기 제거 조건 충족 |

T0의 결과를 반영해 T1 계약을 고정한 뒤 T2/T3/T5를 병렬 개발할 수 있다. T4는 lease protocol과 adapter launch plan을 사용한다. T6 최종 통합은 모든 gate 검증 후 수행. UI 임시 숫자나 fake account를 실제 quota처럼 배포하지 않는다.

### 첫 macOS 릴리스의 사용자 가치

설치형 GUI에서 실제 계정 현황을 보고, 프로필을 연결하며, 두 터미널의 새 세션을 자동/수동 배정하고 실제 계정을 확인할 수 있다. Windows는 후속이지만 GUI·관측·스위칭 중 하나를 나중으로 미뤄 macOS 완료라 부르지 않는다. Google G-GOOGLE이 막히면 해당 범위는 미완료로 명시하고 전체 범위 축소는 사용자 승인이 필요하다.

## 14. 수용 기준과 검증 계획

아래 표는 유지되는 전체 수용 기준이다. 이번에 실행한 증거는 17절에 기록하며, 로컬 빌드와 일부 실사용 검증을 표 전체의 통과로 간주하지 않는다.

| ID | 시나리오 | 통과 조건 |
|---|---|---|
| AC01 | Mac 앱 설치·최초 실행 | 개발 runtime 없이 대시보드·계정 등록·연결 진단 동작 |
| AC02 | Orca와 Terminal 동시 Fable 시작 | 같은 계정 다중 세션 및 서로 다른 계정 각각 검증. 원자적인 동시 슬롯/예측 예산 admission; 공급자 hard quota 확보와 혼동하지 않음 |
| AC03 | 같은 acquire/start 재전송·prepared timeout 경합 | lease/예약/spawnAttempt는 하나. ABORTED는 시작 거부, STARTING timeout은 환급하지 않음 |
| AC04 | 계정 A 사용 중 B 로그인·logout | A의 native identity·session 유지, 전역 credential 덮어쓰기 없음 |
| AC05 | 신규 기본 계정 A→B | next-launch는 B, 기존 A 세션은 A로 표시·유지 |
| AC06 | 수동 B 세션 시작 | 동일 env native identity preflight 통과 전 prompt 전달 안 함; UI에 검증 tier 표시; A 진행 작업 재실행 없음 |
| AC07 | 잘못된 env API key/base URL/helper | known override는 spawn 전 차단; 외부 설정 drift 사후 발견 시 이미 전달된 요청 가능성을 숨기지 않음 |
| AC08 | app 창 닫기·다시 열기 | service와 기존 CLI 유지; 같은 계정/세션 상태 복원 |
| AC09 | 서비스 kill/restart, wrapper kill | 살아 있는 child의 예약 조기 환급 없음; 중복 spawn 없음 |
| AC10 | sleep/wake, 시계 변경, PID 재사용 | 살아 있는 작업을 만료 처리하지 않음; 잘못된 PID kill/계정 해제 없음 |
| AC11 | quota stale/unknown/reset 경계 | 0%나 무한 urgency로 선택하지 않음; 새 window debt와 미래 예약 올바르게 구분 |
| AC12 | 공통·모델 bucket 및 shared pool | Fable 제한과 5h/weekly 모두 검사; 같은 pool을 여러 계정으로 중복 가산하지 않음 |
| AC13 | 반복 snapshot과 지연 반영 | metered 소비는 once-only; estimated는 과거 forecast를 debt로 누적하지 않음. 세션 소비 미관측에서도 정상 배정 지속, 신뢰도 명시 |
| AC14 | OMP pool pin + subagent | 허용 provider identity만 사용; env/API key/누락 provider로 우회하지 않음; 동적 scope 미검증 시 managed 거부 |
| AC15 | Ctrl+C, resize, suspend/resume, pipe JSON | native UX·stdout·exit code 보존; manager 로그로 JSON 손상 없음 |
| AC16 | native background task 생존 | foreground 종료로 slot 즉시 반환하지 않음; 관측 불가면 unknown으로 명시 |
| AC17 | switch 동시 요청/취소/실패 | WAITING은 타깃 hard reservation 없음; source-stop 이후 target launch 직렬화; 새 세션 실패는 기존 프로세스, 인계 실패는 원본 transcript와 resume 경로 보존 |
| AC18 | quota exhaustion·network/401/403 | 오류 분류·복구 안내; 과금·모델 전환·작업 replay 없음 |
| AC19 | 대시보드 정보 정직성 | provider별 bucket/freshness/actual vs desired/공유 여부 표시; 가짜 통합 % 없음 |
| AC20 | 보안 | UI/로그/export/token-free IPC에 token/prompt 없음; 타 사용자 socket 접근 거부; 임의 binary 실행 거부 |
| AC21 | updater·제거 | live session 유지; schema 호환 검증; 원본 CLI·사용자 credential/설정 보존 |
| AC22 | Google 두 계정 동시 실행 | keyring/backend 포함 identity 분리와 quota attribution 증명. 실패하면 Google 자동 전환 완료 금지 |
| AC23 | 전체 공급자 실사용 | Anthropic/OpenAI/Google/xAI 각각 native 실행·quota·수동 선택 evidence. 소진 계정·미보유 2계정은 그 검증의 외부 prerequisite로 기록 |
| AC24 | launcher 우회 | 사용자가 지정한 통합 경로의 우회 상태를 표시; Mac 전체 완전 감시/차단을 주장하지 않음 |

실제 macOS 앱·Orca·Terminal의 설치/화면/스위칭은 native surface로 확인한다. 순수 scheduler는 fake clock과 의미 있는 shared pool/동시 요청 fixture로 회귀 테스트. provider quota 소진을 강제로 만들지 않고 parser fixture와 한도가 허용된 최소 live 호출로 분리한다. live inference는 사용자에게 소모 범위를 알리고 실행한다.

## 15. Release gates와 남은 확인 사항

| Gate | 현재 상태 | 통과 기준 |
|---|---|---|
| G-POLICY | provider별 검토 필요 | 공식 auth 경로/약관 범위 확인. 제한 우회 목적 pooling을 허용 기능으로 설명하지 않음 |
| G-IDENTITY | Claude 기본 프로필 preflight와 실실행 확인; Codex 로그인 필요, Grok identity 조회 미확인 | 공급자별 actual identity probe와 두 프로필의 로그인·logout 격리 실증 |
| G-POOL | 같은 provider·identity의 슬롯 공유 구현; 독립 entitlement 보증은 미완료 | provider entitlement scope 기반 공유 관계 확인. 일치/불일치 %로 추론하지 않음 |
| G-QUOTA | OMP usage 정규화·native 프로필 연결 구현; 없는 값은 unknown | 전체 adapter의 모델별 quota·freshness 실사용 관측 |
| G-OMP | 프로세스·저장된 계정/모델 기록·확장 선택 상태 관측 활성; 관리 실행 차단 | API-key 우회·동적 provider·profile broker 상속을 막는 effective launch scope 검증 |
| G-GOOGLE | **현재 미해결** | agy 프로세스별 credential/keyring/backend 격리의 지원 수단 발견·실증 또는 사용자 승인된 대체 실행 환경 |
| G-SWITCH | Claude 신규 실행·동일 계정 대화 재개 실증; 교차 계정 인계 비활성 | 두 native 계정의 전환·격리 증거와 실패 보존. cross-account resume은 지원 입증 전 비활성 |
| G-DISTRIBUTION | arm64 앱·DMG 로컬 ad-hoc 서명 빌드 확인 | Developer ID/notarization, 배포·업데이트 서명과 실제 endpoint, 깨끗한 Mac 설치 |

이 문서는 구현 계약을 확정하지만 위 gate가 이미 통과됐다고 주장하지 않는다. 특히 Google 다중 계정 동시 전환과 모든 native 세션의 무중단 hot-swap은 현재 근거로 보장할 수 없다. 안전한 새 세션 선택과 지원되는 전환을 제품 기본 동작으로 삼고, 불가능한 즉시 전환 버튼을 만들지 않는다.

## 16. 참고 자료

확인일: 2026-09-20. main branch 자료는 변동 가능하며 실제 지원 판정은 설치 CLI 버전에 연결한다.

1. OMP 18.2.6 계정 순위: https://github.com/can1357/oh-my-pi/blob/v18.2.6/packages/ai/src/auth-storage.ts#L4975-L5038
2. OMP broker/pool: https://github.com/can1357/oh-my-pi/blob/v18.2.6/docs/auth-broker-gateway.md 및 설치 번들 `omp://auth-broker-gateway.md`
3. Claude auth/Keychain namespace: https://code.claude.com/docs/en/authentication#credential-management
4. Claude env/profile: https://code.claude.com/docs/en/env-vars
5. Claude quota statusline: https://code.claude.com/docs/en/statusline#rate-limit-usage
6. Anthropic credential policy: https://code.claude.com/docs/en/legal-and-compliance
7. Codex auth: https://developers.openai.com/codex/auth
8. Codex app-server: https://developers.openai.com/codex/app-server
9. OpenAI terms: https://openai.com/policies/row-terms-of-use/
10. Grok settings: https://docs.x.ai/build/settings/reference
11. Grok CLI: https://docs.x.ai/build/cli/reference
12. CodexBar Grok integration limitations: https://github.com/steipete/CodexBar/blob/main/docs/grok.md
13. Google CLI transition: https://developers.googleblog.com/an-important-update-transitioning-gemini-cli-to-antigravity-cli/
14. Antigravity auth: https://antigravity.google/docs/cli/install
15. Antigravity reference: https://antigravity.google/docs/cli/reference
16. agy community switcher, not adopted: https://github.com/alienyst/antigravity-cli-switch-account
17. CLIProxyAPI reset-aware request, not planned: https://github.com/router-for-me/CLIProxyAPI/issues/4715
18. Apple LaunchAgents: https://developer.apple.com/library/archive/documentation/MacOSX/Conceptual/BPSystemStartup/Chapters/CreatingLaunchdJobs.html
19. SQLite transactions: https://www.sqlite.org/lang_transaction.html
20. Tauri distribution: https://v2.tauri.app/distribute/
21. Tauri security capabilities: https://v2.tauri.app/security/capabilities/
22. Antigravity quota panel: https://antigravity.google/docs/cli/commands/usage
23. Antigravity settings: https://antigravity.google/docs/settings?tab=cli
24. Tauri macOS signing/notarization: https://v2.tauri.app/distribute/sign/macos/
25. OMP 18.2.6 credential pin: https://github.com/can1357/oh-my-pi/blob/v18.2.6/packages/coding-agent/src/session/credential-pin.ts
26. OMP 18.2.6 session entry: https://github.com/can1357/oh-my-pi/blob/v18.2.6/packages/coding-agent/src/session/session-entries.ts

## 17. 구현 산출과 실제 검증

### 17.1 현재 산출

- 독립 Rust workspace: framed JSON-RPC/사용자 전용 UDS, SQLite 단일 writer 서비스, 계정·quota adapter, native CLI launcher.
- Tauri macOS 앱: Overview/Accounts/Sessions/Policies/Connections, 개인정보 가림, 메뉴바 상태·배정 제어, 실제 서비스 응답 기반 배정 미리보기와 Terminal 실행.
- 정책: 원자적 슬롯 배정, 요청 멱등성, generation/spawnAttempt fencing, 실제 프로세스 식별, 종료가 불확실한 작업의 보수적 점유, 프로젝트 경로 경계·symlink 검사, CAS 변경.
- 계정 연결: 공식 CLI 로그인과 profile binding. Claude/Codex의 안전한 선언형 설정만 사전 diff·digest 확인 후 새 프로필에 복사할 수 있다. 인증·MCP·hooks·plugins·skills·대화 기록은 복사하지 않으며 생략 항목을 표시하고 동의받는다.
- 진단 내보내기: 허용된 상태 필드만 사용하며 계정·세션은 내보내기별 임시 번호로 치환한다. 이메일·identity·프로필/작업 경로·native 세션 ID·원본 오류·토큰·프롬프트는 제외한다.
- 설치 산출: `target/release/bundle/macos/AI Account Manager.app`, `target/release/bundle/dmg/AI Account Manager_0.1.0_aarch64.dmg`.
- UI에서 사용자 LaunchAgent 설치·시작과 안전한 중지를 제공한다. CLI의 `service uninstall`은 앱 소유 등록만 제거한다. shim 설치는 전용 디렉터리에 한정하며 PATH와 기존 CLI를 자동 변경하지 않는다.
- `aam shell install`은 사용자 승인 후 `.zshrc`에 소유권이 확인되는 PATH 블록만 추가하고 원본을 보존한다. `aam shell uninstall`은 기록된 블록만 되돌린다. 실행 가능한 등록 계정이 없는 도구는 신규 shim으로 가로채지 않는다.
- 현재 `~/Applications/AI Account Manager.app`과 사용자 LaunchAgent, `aam`·`claude`·`omp` shim, 기존 zsh 연결을 유지한다. 새 터미널에서 `command -v claude`·`command -v omp`를 확인하고, 기존 zsh는 종료하지 않고 `rehash` 후 확인할 수 있다. Orca 명령 재정의·기본 인수·선택 프로필은 변경하지 않았다. 원격 호스트와 별도로 지정한 원본 절대 경로는 이 PATH 연결 범위 밖이다.
- 기존 OMP는 재시작 없이 `observedSessions`로 발견한다. 관리 lease·슬롯과 분리하고, 조회 실패/부분 관측을 정상적인 0개와 구분한다. Overview와 Sessions에 관리/외부 관측 수를 따로 표시하며 개인정보 가림은 외부 cwd·관측 상세에도 적용한다. 외부 행에 종료·재개·전환 기능을 부여하지 않는다.
- `ObservedSession.attributions`는 main/subagent/auxiliary별 provider·model·계정 참조·근거·시각·stopReason을 담는다. `session-pin`/`session-selection`만 계정과 연결하고 `configured` 모델 설정과 미확인 호출 기록은 분리한다. 구버전 서비스의 필드 누락을 지원하며 진단 export에 pin hash·세션 식별자를 추가하지 않는다.
- 연결 상태 화면에서 OMP 관측 확장을 설치·제거·확인한다. CLI는 `aam omp-observer status|install|uninstall`이다. native launcher에 내장된 단일 JS를 소유권 receipt·digest와 함께 설치하며 수정된 기존 파일·symlink 충돌은 덮어쓰지 않는다. 개발용 Node가 없어도 설치할 수 있다.
- 실제 기본 프로필의 `~/.omp/agent/extensions/aam-observer/index.js`를 설치했다. 이후 시작하는 OMP에 자동 적용하며 실행 중인 작업은 reload/restart하지 않는다. 별도 프로필과 `--no-extensions` 실행은 이 적용 범위 밖일 수 있다. 실제 요청 계정 확인이나 계정 고정 기능으로 표시하지 않는다.
- 세션 UX는 분수형 `0 / 5+` 대신 `관리 0개`, `외부 5개 확인`을 구분하고 부분 조회는 `일부 미확인`으로 표시한다. 정상 0·전체 조회 실패·구버전 미지원·오프라인의 마지막 응답을 구분한다. 시작 버튼은 `새 관리 세션`으로 통일했으며 OMP 자체 계정 선택과 이 앱의 관리 실행을 구분한다. 이 수정은 배정 정책이나 OMP 인증을 변경하지 않는다.
- OMP 계정 고정 실행: 설치된 서비스에서 비밀 없는 metadata로 확인한 7개 OAuth 연결(Anthropic 3개)을 승격한다. UI의 계정 inspector → 새 관리 세션, 관리 `omp` shim과 `aam run omp`가 앱 소유 sidecar를 실행한다. 새 작업은 프로젝트·저장소 규칙과 명시한 계정/모델을 적용하며, 관리 대화 재개는 원래 binding에 고정한다. 모델 공급자 불일치와 다른 계정 대체를 차단한다.
- 관리 실행본은 native addon을 앱 전용 cache에 추출하고 standalone `createRequire`의 기준을 설치 실행본으로 둔다. LaunchAgent가 빌드 당시 Documents 경로를 다시 참조하는 문제를 수정했으며, Full Disk Access 추가나 전역 Bun/OMP 변경으로 우회하지 않았다.
- 배정 정책에 프로젝트·워크트리 규칙의 추가·수정·삭제와 CAS 충돌 안내를 제공한다. 연결 상태에서는 6개 호스트의 설치 감지·정적 설정·실행 방식별 명령과 한계를 구분한다. 호스트 설정을 자동 작성하지 않으며 정적 상태를 실제 실행 성공으로 표시하지 않는다.
- 배정 정책 상단에 스마트/우선순위 두 옵션과 계정 위·아래 이동을 제공한다. 모드와 순서를 한 번에 저장하고, 충돌 시 초안을 보존하며 최신 정책 확인·취소 경로를 제공한다. 스마트 전환은 저장 순서를 지우지 않는다. 계정명 중복은 도구·가림 처리한 신원·짧은 ID로 구분한다. 기존 도구/프로젝트 pin은 고급 예외로 접어 두고 모드 변경으로 자동 해제하지 않는다.
- 외부 대화 인계: 세션 화면의 외부 관측 행에 `관리로 인계`·`인계 취소`와 소유 계정 근거를 제공하고, 대화 파일을 확정하지 못한 행은 `인계 대상 확인 불가`로 구분한다. 배정 정책에 `외부 대화 자동 인계` 설정을 둔다. OMP 관측은 최상위 프로세스가 쓰는 현재 대화 파일만 후보로 제시하며 확장·보조 세션 파일은 제외한다.
- 관리 배정 실패는 실행을 막지 않는다. 관리 shim이 이유를 알린 뒤 원본 CLI로 통과시키고, 통과 실행은 관리 세션으로 기록하지 않는다. 모델 미지정 실행은 공통 한도만으로 판정하고 공식 설정에서 확인한 기본 모델의 소진 한도는 순위에만 반영한다.
- 계정 구분: 확인된 이메일을 계정 이름에 함께 표시한다. 자동 생성된 이름만 갱신하고 사용자가 바꾼 이름은 유지한다. Codex는 `account/read`가 계정 식별자를 주지 않으므로 공식 auth 파일의 비밀 아닌 `tokens.account_id`를 함께 확인하고, 없으면 이메일과 선택한 `CODEX_HOME`으로만 구분한다(같은 이메일의 여러 워크스페이스는 구분하지 않음). 토큰 값은 읽지 않는다.
- Codex 비대화형 실행(`codex exec`)을 관리 실행으로 허용한다. 인증·설정·프로필을 바꾸는 하위 명령(`login`, `logout`, `config`, `app-server` 등)은 계속 차단한다.
- 계정 화면은 확인된 신원 기준으로 한 줄에 한 계정을 보여준다. 워크스페이스·subject·OMP OAuth pin이 같은 도구 연결을 묶고, 워크스페이스를 확인하지 못한 연결만 이메일로 연결한다. 같은 이메일의 다른 워크스페이스는 합치지 않으며, 신원 근거가 없는 연결은 따로 표시한다. 사용량·리셋은 계정 줄에 한 번만 표시하고, 도구별 연결은 하위 행에서 인증 상태·동시 세션 상한·설정 열기로 구분한다. 서로 다른 계정에서 같은 버킷이 관측되면 `공유 관계 미확정`으로 표시한다.

### 17.2 실행해 확인한 증거

로컬 검증 산출: `artifacts/verification.json`에 패키지 SHA-256·검증 범위·정리 결과를 기록한다. 기존 화면과 개인정보 제거 진단에 더해 `desktop-omp-attribution.webp`, `desktop-omp-attribution-details.webp`, `desktop-omp-integration.webp`에 업데이트한 실제 앱의 가림 상태를 보관했다. 검증 후 사용자의 원래 개인정보 표시 설정으로 복원했다.

| 영역 | 실제 결과 | 범위 제한 |
|---|---|---|
| 빌드·정적 검사 | Rust 60개·OMP 확장 5개 테스트, TypeScript 검사, Clippy `-D warnings`, 앱·DMG 빌드, 설치본 서명·DMG 무결성 확인 | 전체 AC 통과나 다른 Mac 검증은 아님 |
| 동시 배정 | 슬롯 1개에 12개 동시 요청 시 1개만 배정; 동일 요청 12회는 동일 lease로 수렴 | 공급자의 hard quota 예약은 아님 |
| 복구·제거 경합 | daemon 재시작 후 기록 유지, 중복 daemon 거부, 활성 lease의 중지 거부, 제거 준비 중 신규 배정 거부 | 실제 sleep/wake·OS 시계 변경은 수행하지 않음 |
| Claude 실실행 | 기존 구독 계정 하나에서 `haiku` 2개 동시 응답과 동일 native 대화 재개 1개 성공 | 총 3개의 짧은 응답. 서로 다른 계정·Fable 실추론 검증은 아님 |
| 실제 Terminal | 앱에서 새 Terminal 창 실행, preflight 후 ACTIVE 확인, 앱 창 닫기·재열기 후 같은 native 프로세스 유지 | 기존 사용자의 Terminal 창은 조작하지 않음 |
| 기본 모델·Orca 연결 | 실제 Terminal의 `claude`와 Orca의 기존 `claude --dangerously-skip-permissions`가 Fable 5.1로 ACTIVE 진입. 각각 `/exit` 후 코드 0·EXITED·점유 0 확인 | 모델 인수를 추가하거나 추론 프롬프트를 보내지 않음. 두 실행은 순차 검증 |
| 기존 OMP 발견—이전 검증 | 설치된 실제 앱에서 기존 Orca OMP 5개 표시. 별도 Terminal의 비영속 RPC OMP 시작 시 6개, EOF 종료 시 5개로 자동 갱신. 실제 usage 조회 중에도 17개 상태 응답 모두 5개 유지 | 이 단계는 추론 없는 프로세스 관측 검증. 아래 계정 연동 검증과 구분 |
| 기존 OMP 계정 기록 | 업데이트한 앱·서비스에서 기존 5개 중 4개의 계정 기록 연결. 실제 계정 표시 readback과 가림/상세 화면 확인. 5개 PID/birth·기존 배정 정책 유지, 관리 점유 0 | 쓰기용 세션 FD를 확인하지 못한 1개는 미확인. 현재 요청의 인증 계정을 증명하지 않음 |
| OMP 확장 실실행 | 독립 검증 세션에서 `gpt-6-astra`의 `OK` 응답 1회. 확장→서비스의 subagent 계보, pin·선택 계정 연결, 별도의 `stop` 호출 결과 확인. EOF·shutdown 확인 | 추가 프롬프트는 이 1회뿐. 호출 행 accountId는 null 유지 |
| 확장 설치·자동 로드 | 실제 UI로 설치 후 native status에서 installed/current 확인. `-e`·AAM_HOME override 없이 새 기본 OMP가 자동 로드하고 실제 manager 경로에 0600 snapshot 기록. 추론 없이 종료 | 기존 OMP에 소급 로드하지 않음. 별도 프로필·모든 OMP 버전 검증은 아님 |
| 확장 설치 안전성 | 격리 환경에서 설치·상태·멱등성·제거, custom AAM_HOME 내장, 0600 권한, 수정 파일 충돌 거부·보존 확인. 인증 DB 생성 없음 | 이 결과를 OAuth 인증 격리 또는 관리 실행 검증으로 대체하지 않음 |
| 자손 종료 처리 | MCP 8개·hooks 13개를 쓰는 실제 CLI에서 정상 종료·점유 반환 확인. 일시적 식별 실패 후 확정 소멸 재확인과 저장된 자손 identity·원래 프로세스 그룹에 따른 지연 반환은 회귀 테스트로 검증 | 수정 후 실제 실행에서 일시적 식별 실패를 재발시킨 것은 아님. 근거 없는 기존 ORPHANED는 자동 해제하지 않으며 원격 daemon·관측 밖의 setsid/double-fork 후손 소멸은 보장하지 않음 |
| TTY | 80×24→190×59 resize, Ctrl+Z 정지와 fg 재개, raw mode와 점유 유지, Esc 취소 후 native·셸 종료 코드 0 및 EXITED 확인 | Ctrl+C의 공식 CLI 재입력 안내는 관측했으나 두 번 입력에 의한 종료까지는 검증하지 못함 |
| 프로젝트 범위 | 허용 폴더는 선택, 외부 폴더·내부에서 외부로 향하는 symlink는 거부 | OS sandbox 기능은 아님 |
| 설정 보존 | 실제 Claude 설정 미리보기, 민감 항목 제외, stale digest·기존 파일 덮어쓰기 방지 검사 | 새 계정의 브라우저 로그인은 사용자 수행 필요 |
| 진단 저장 | macOS 저장 대화상자 취소·저장, 실제 JSON의 민감 metadata 부재와 0600 권한 확인 | 명시적으로 수집하지 않은 실행 내용은 포함하지 않음 |
| 설치 수명 | 최초 설치·중지·재시작·제거 왕복 검증 후 사용자 요청으로 실제 앱·LaunchAgent·shim·zsh 연결 설치. 원본 `.zshrc` 내용과 Claude 설정, Orca 실행 설정 보존 확인 | 현재 설치는 유지. 원본 Codex·OMP 바이너리는 보존하며 새 `omp`의 PATH 해석에는 추가 shim이 적용됨. 개발 중 생성한 실패 재현 예약 2건만 프로세스 소멸 확인·비공개 백업 후 제거 |
| 설치 소유권 | foreground daemon을 설치 완료로 오인하는 상황 재현 후 차단. 수정 후 `UNMANAGED_SERVICE`, 새 plist 없음, 기존 daemon·세션 유지 확인 | 직접 실행한 서비스는 실행한 터미널에서 종료해야 함 |
| 앱 화면 | 실제 macOS 창·계정 inspector·연결·정책 화면, 자동 배정 일시정지/재개 확인. 모델을 비운 새 세션 창에서 실제 배정 확인 후 시작 버튼 활성화 확인 | 메뉴바 항목 직접 클릭과 최소 창 크기 검증은 미실시 |
| 관측 실패·개인정보 | 실제 앱에서 외부 경로 가림과 5개 행 표시 확인. 실제 컴포넌트의 일회성 서버 렌더링으로 구버전 응답·조회 실패·부분 관측·민감 경로/근거 가림 검증 | 실패 상태를 native 앱에 강제로 주입한 시험은 아님 |
| 세션 집계 UX | 설치 앱 1180×780에서 명시적 관리/외부 집계·부분 조회 안내, 새 관리 세션 창 열기/취소 확인. 실제 컴포넌트로 정상·부분·실패·정상 0·구버전·오프라인·개인정보 상태 확인. 기존 OMP 5개와 배정 정책 유지 | native 최소 창 크기 변경은 확인하지 못함. 정책 엔진·인증·OMP 관리 지원은 변경하지 않음. 화면은 `desktop-session-counts.webp`, `desktop-session-counts-partial.webp` |
| OMP 계정 고정 회귀 | Rust 69개, private runtime 22개 테스트 및 TypeScript 검사 통과. A 차단·삭제·중복·refresh identity 변경, API/config/runtime/fallback 우회, provider/endpoint override, worker 정책 전달·분리와 실제 schema 7 저장소의 읽기·시작 검증 | fixture의 한도 소진·변조 검증이며 실제 B/C 구독을 소진시키거나 모든 공급자에서 추론한 것은 아님 |
| OMP 설치 서비스 | 최종 LaunchAgent 조회에서 관리 연결 7개·Anthropic 3개 확인. 설치본과 배포 bundle의 runtime SHA-256 일치, 서명·DMG 무결성 확인 | 기존 OMP 5개의 PID/birth, 원본 바이너리와 배정 정책 유지. 인증 DB 직접 복제 없음 |
| OMP 실제 실행 | 최종 일반 설치본에서 명시한 Anthropic 계정의 `claude-haiku-4-5` 응답 `OK`, 종료 코드 0·EXITED·점유 0 확인. UI에서 같은 계정으로 별도 Terminal을 열어 Haiku 4.5 준비 화면 확인 후 `/exit` 코드 0·점유 반환 확인 | Terminal 확인에서는 추론하지 않음. 원래 Terminal 창은 유지. 다른 공급자의 실추론 검증은 아님 |
| OMP UI 제한 | 실제 창에서 공급자가 다른 모델은 배정·시작 불가, 지정 계정만 후보, 다른 계정은 `ACCOUNT_PINNED` 제외 확인. 가림 화면은 `desktop-omp-hard-control.webp` | 당시 UI 직접 실행 검증이며, 이후 새 `omp` shim의 공통 정책 연결은 아래 별도 검증에 기록 |
| OMP 초기 종료 이상 | 초기 실추론 1회는 `OK` 뒤 78로 종료했으며 원인은 미확정이다. 이후 호출 위치만 기록하는 진단 빌드와 최종 일반 빌드에서는 각각 코드 0·점유 반환 확인 | 재현하지 못했으므로 수정 완료나 잘못된 차단으로 단정하지 않는다. 초기 종료 후 남은 테스트 소유 외부 도구 자손은 식별 후 정리했다. 다른 세션에는 신호를 보내지 않음 |
| OMP 검증 환경 | 첫 비대화형 검증은 닫히지 않은 inherited stdin의 EOF를 기다렸다. 명시적 빈 stdin으로 다시 실행해 실제 SDK 경로를 검증했다. 저장소 초기화 오류는 실패하는 실제 DB 회귀로 재현 후 readwrite/create 옵션과 TEMP revision 초기화로 수정 | EOF 대기는 테스트 입력 방식 문제로 구분한다. 상속된 일부 MCP 설정의 연결 실패는 관측했으나 사용자 설정을 변경하지 않았다. 임시 transport fixture·지연·callsite 로깅은 배포하지 않음 |
| 공통 호스트 회귀 | 최종 Rust 93개, 설치된 private OMP runtime 27개·95 assertions, TypeScript·Clippy·앱/DMG 빌드 통과 | 워크트리·상속·충돌·실패 경계는 격리 회귀이며 실제 다중 공급자 추론을 대신하지 않음 |
| Orca 터미널 | 일반 `omp`·`claude`의 새 실행과 같은 native 대화 재개가 각각 응답·코드 0·점유 반환. 최종 설치본의 OMP `--continue`와 Claude UUID 재개도 이전 검증 토큰을 그대로 회상 | Orca 일반 터미널 CLI 검증. 내장 native/structured chat, 별도 계정 선택 UI 경로의 실검증은 아님 |
| cmux 터미널 | 일반 Claude 명령이 실제 cmux hook·UUID·preload wrapper를 통과해 새 실행·재개 성공. OMP 새 실행·native UUID 재개도 코드 0·같은 계정·대화 유지 | ancestry-only socket 정책 유지. 자체 OMP sidebar 확장은 설치·검증하지 않음 |
| Claude 자기 재실행 | 최종 cmux Claude의 실제 Bash 자식에서 공식 process wrapper로 native `--version` 실행: 12ms·코드 0·stderr 없음. `CMUX_CLAUDE_PID`가 실제 native 실행 파일을 가리킴 | 같은 root 문맥의 wrapper 직접 호출 검증. agent teams·장수 background daemon의 자동 self-spawn 전체 검증은 아님 |
| 모델 우선순위 | 추론 전에 실패하는 native format 옵션으로 프로젝트 haiku가 명시한 sonnet을 덮는 문제 재현. 수정된 설치본의 실제 예약 모델은 sonnet | native 종료 1은 의도한 인수 파싱 실패이며 추론하지 않음 |
| 관리 제외·실패 경계 | 명시적 unmanaged 하위 경로의 Claude unknown UUID·OMP 외부 파일 요청은 원본과 종료 코드/출력이 일치하고 lease 없음. 서비스 불가 시 우회 거부, 알려진 Claude 관리 대화는 같은 경로에서도 원래 계정 lease 유지 | native 인수 오류와 OMP help를 이용한 무추론 검증. 관리 제외에는 계정 고정 보장을 부여하지 않음 |
| 등록 시 규칙 보존 | 격리 HOME·가짜 metadata CLI로 identity 없는 같은 프로필을 공식 등록할 때 pin·선호 계정·허용 목록이 새 binding으로 함께 이동하고 후속 경로가 이를 선택함 | 실제 인증·토큰·기존 계정 저장소를 사용하지 않은 서비스 실실행 검증 |
| 새 앱 화면 | 실제 설치 앱에서 프로젝트 규칙 저장·readback, 계정 구분 표시, 편집 취소, 6개 호스트의 정적 상태·한계 표시 확인. `desktop-host-policies.webp`, `desktop-host-connections.webp`에 이메일 가림 상태 기록 | 테스트 경로는 임시 디렉터리. 정적 호스트 표시를 실행 성공으로 자동 승격하지 않음 |
| OMP 종료 경계 | 닫힌 auth store에 대한 늦은 요청을 별도 회귀로 재현하고 request 경계의 store 유지로 수정 | 과거 최초 실응답 후 78 종료의 원인으로 확인된 것은 아니며 해당 미확정 기록을 유지 |
| 계정 배분 두 모드 | Rust 99개, 설치된 OMP runtime 27개·95 assertions, TypeScript·Clippy·앱/DMG 빌드 통과. 새 설치 서비스가 기존 정책을 smart/빈 순서로 읽음 | 최초 Clippy의 테스트 초기화 지적 3건을 수정한 최종 코드 기준 |
| 계정 배분 실제 화면·서비스 | 설치 앱에서 우선순위 선택·계정 아래 이동·저장 확인. 실제 `route.explain`이 첫 적격 계정을 선택하며, 반대 순위를 주었을 때 스마트와 우선순위가 서로 다른 적격 계정을 선택함. 스마트 복귀 시 저장 순서는 유지하되 선택에서는 무시함 | 실제 계정 metadata에 대한 무추론 배정 미리보기. 구독을 실제로 소진시키거나 장기 분배 효율을 측정한 것은 아님 |
| 배분 충돌·정리 | 편집 중 외부 policy 갱신으로 오래된 저장 차단·초안 보존·취소 확인. 검증용 순서를 제거하고 최종 smart/빈 순서 유지. 기존 기타 정책·원본 설정·OMP 5개 유지, 서비스 정상 연결·점유 0. `desktop-allocation-smart.webp`, `desktop-allocation-priority.webp` 기록 | 실제 우선순위 소비는 도구에 연결된 계정 단위이며 실행 중 계정 회전은 하지 않음 |
| 모델 한도 경계 | 사용자가 보고한 `claude --dangerously-skip-permissions` 차단을 재현하고, 수정 후 같은 실행이 배정됨. 실제 원인은 계정 프로필 설정의 기본 모델 `claude-fable-5-1[1m]`과 소진된 Fable 주간 한도이며 `MODEL_LIMIT_EXPECTED`로 표시. 같은 계정에 `--model sonnet`을 지정한 실제 실행은 코드 0·정상 응답 | Fable 한도 자체는 공급자 상태이며 앱이 해제하지 못함. 셸 환경 변수·CLI 인수로 지정한 모델은 사전 확인 범위 밖 |
| 외부 Claude 대화 인계 | 원본 CLI로 만든 외부 대화(UUID)를 관리 shim에서 재개: 자동 인계로 소유 계정(alice)에 고정된 관리 세션 생성, 이전 토큰 회상, 코드 0, `preflight-verified` | 검증용으로 만든 외부 대화이며 사용자 기존 전사는 사용하지 않음 |
| 외부 OMP 대화 인계 | 실제 프로필의 단일 공급자 대화를 `aam takeover adopt`로 인계(pin→계정 근거 기록) 후 관리 shim `--session` 재개가 소유 계정 관리 세션으로 실행·코드 0. 여러 공급자 대화·관리 대화·프로필 밖 경로는 거부 | 다중 공급자 대화는 설계상 인계 대상 아님. 실행 중 프로세스를 인수하지 않음 |
| 관리 불가 통과 | 안전 여유량을 99%로 올려 모든 계정을 제외시킨 실제 상태에서 관리 shim이 `SAFETY_RESERVE` 사유를 알리고 원본 `claude`를 실행해 코드 0·정상 응답. 검증 후 여유량 5%로 복원 | 통과 실행은 관리 세션·슬롯에 포함되지 않음 |
| Codex 계정 인식 | 사용자가 추가한 Codex 로그인 2개가 등록되지 않던 원인(설치된 CLI가 계정 식별자 미제공)을 확인하고, 이메일·`tokens.account_id` 기준으로 두 계정을 실제 등록. `aam explain --tool codex`가 0% 계정(project-a)을 선택하고 100% 계정(bob)을 소진으로 제외. 관리 shim `codex exec` 실제 실행 코드 0 | 같은 이메일의 여러 워크스페이스 구분은 미확인. `~/.codex` 기본 프로필은 로그인 없음으로 유지 |
| 계정 이메일 표시 | 실제 설치 앱과 서비스에서 OMP·Claude·Codex 계정 이름에 확인된 이메일이 함께 표시됨. `desktop-account-emails.webp`, `desktop-takeover-sessions.webp`, `desktop-takeover-policy.webp` 기록 | 이메일 없는 연결(미로그인 Codex, identity 미지원 Grok, 재관측 실패 OMP)은 그대로 표시 |
| 계정 화면 묶음 | 실제 설치 앱에서 연결 14개가 확인된 신원 기준 10개 계정으로 묶임. 같은 계정의 Claude Code·OMP 연결이 한 줄에 표시되고 하위 행에서 도구별 설정이 열림. 같은 이메일의 다른 워크스페이스(Anthropic project-a/project-b, OpenAI project-a/bob)는 분리 유지. `desktop-account-groups.webp` 기록 | 양쪽 모두 워크스페이스를 모르는 같은 이메일 연결의 묶음은 미확인 |

앱·launcher·서비스의 동적 링크는 macOS 시스템 라이브러리만 참조하는 것을 확인했다. 앱 자체를 실행하는 데 개발용 Node/Rust는 필요하지 않지만, 연결할 공식 CLI와 그 CLI의 실행 환경은 별도로 필요하다. 깨끗한 다른 Mac에서의 설치 시험을 대신하지는 않는다.

### 17.3 전체 완료로 표시하지 않는 항목

- Codex: 현재 native 로그인이 없어 실제 identity·quota·실행·두 계정 검증이 남아 있다.
- Grok: 설치 CLI의 비밀 없는 안정적 subject 조회를 확인하지 못했으며 quota 소진 조건도 있다. 검증 없이 관리 실행을 허용하지 않는다.
- Google/agy: 프로세스별 Keychain·backend 격리 근거가 없어 자동 전환을 차단한다. 다른 CLI나 격리 실행 환경으로 대체하려면 별도 사용자 결정이 필요하다.
- OMP: 앱 소유 실행본의 정상 SDK 경로에서는 단일 OAuth 고정을 제공한다. 원본 외부 OMP의 개별 요청 identity는 관측 기록만으로 확정하지 않는다. 실제 응답 검증은 Anthropic 한 계정에 한정하며 모든 공급자·동적 외부 실행의 upstream identity를 증명하지 않았다. 초기 응답 후 78 종료 1회의 원인도 미확정이다. G-OMP/AC14 전체 통과로 표시하지 않는다.
- Claude: 시작·재개·자식 실행의 프로필/계정 문맥을 확인하지만 프로세스 내부 `/login`이나 외부 인증 변경을 영구 봉쇄하는 기능은 아니다. 두 native 프로필의 실제 로그인·logout 격리와 A→B 신규 기본 계정 전환은 두 번째 계정 연결 후 확인해야 한다. 기존 OMP credential을 복사해 이를 대신하지 않는다.
- 교차 계정 대화 인계와 무중단 hot-swap은 제공하지 않는다. 같은 계정의 지원되는 native 재개와 별도 새 세션 실행을 구분한다.
- 호스트: 실제 실행은 Orca·cmux의 로컬 터미널에서 확인했다. Superset·Conductor·VS Code는 미설치, Cursor는 설치·정적 설정만 확인했으며 실실행은 미검증이다. 각 호스트의 내장 chat/SDK, Claude VS Code 확장 wrapper, 원격 런타임과 임의 프로필까지 지원했다고 표시하지 않는다.
- 서명된 자동 updater는 제공하지 않는다. 실제 배포 endpoint·업데이트 서명 및 Apple Developer ID/notarization 자격이 없는 상태에서 자동 배포·업데이트 완료를 주장하지 않는다.
- Windows 구현, OS sleep/wake·시계 변경, 실제 장수 background task와 wrapper 강제 종료 조합의 전체 실기 검증은 남아 있다.
- 별도 원격 저장소 생성, 커밋, 푸시, 외부 배포는 수행하지 않았다.

## 18. 교차검토와 반영

advisor Codex 경로는 `preflight:not-logged-in`으로 실행되지 않았다. 기존 로그인 설정을 변경하지 않고 Claude Code read-only 검토를 수행했다. 반환된 Critical 2개·Major 3개를 문서 계약과 대조해 반영했다.

- spawn 전 durable STARTING CAS와 generation/attempt fencing을 추가했다.
- 관측 불가능한 세션 소비를 debt로 누적하는 계약을 제거하고 metered/estimated 모드를 분리했다. 리뷰의 “2회 신선한 poll이면 debt 제거” 제안은 포함 근거를 보장하지 않아 채택하지 않았다.
- provisional pool과 불확실한 공유 관계를 별도 admission 제약으로 분리하고 mapping 변경은 drain 뒤 수행하도록 정했다. OMP opaque identityKey를 명시했다.
- 첫 prompt 전 preflight identity와 사후 runtime/upstream 확인 수준을 구분해 실행 전 보장과 UI 표시를 일치시켰다.
- 작업 인계의 source-stop/snapshot/target-launch를 직렬화하고 대기 중 타깃 hard reservation을 없앴다. 실패 시 프로세스 보존과 transcript 보존의 의미를 구분했다.

이는 문서 리뷰 결과이며 실제 계정 전환·동시 실행·설치 테스트 통과를 의미하지 않는다.

기존 OMP 관측 수정의 설치 전 advisor 호출은 Codex 로그인 없음, Grok 잔액 소진, Gemini 유효 JSON 응답 없음으로 모두 검토 결과를 얻지 못했다. 이를 승인으로 간주하지 않았으며 직접 코드 검토·실제 OS/API·native 화면과 회귀 테스트로 확인했다. 그 과정에서 서비스의 `omp usage`가 외부 세션으로 오인되는 문제를 실재현하고, 실패하는 회귀 테스트를 추가한 뒤 검증된 서비스 조상 관계로 제외했다.

추가 계정 연동의 설치 전 Gemini 검토도 빈 응답으로 유효한 판단을 얻지 못했다. 외부 검토 완료로 기록하지 않았다. Rust/확장 회귀 테스트와 Clippy, 실제 OMP의 단일 응답, 설치·자동 로드, native 앱의 계정 readback·개인정보 가림으로 직접 검증했다.

계정 고정 실행에서는 독립 코드 검토와 직접 검증으로 symlink launcher 경로, native/OMP 공유 동시 실행 상한, 식별 모호성 및 UI 보장 문구를 보완했다. 외부 advisor는 Codex 로그인 없음·Grok 잔액 소진·Gemini 유효 응답 없음으로 승인 근거를 얻지 못했다. 추가 scout 두 건도 quota 오류로 결과가 없었으며 검토 완료로 계산하지 않는다. 설치 환경에서 발견한 native loader의 빌드 경로 참조와 SQLite 연결 초기화는 직접 재현·수정했다. 마지막 일반 배포본의 실응답·UI Terminal 종료 성공과 초기 원인 미확정 78 종료를 함께 기록한다.

공통 호스트 연결에서는 읽기 전용 route 검토가 모델 우선순위·관련 없는 stale 폴더·안전한 등록 시 pin 이관·ID namespace 충돌을 지적했고 회귀와 실실행으로 수정했다. 별도 launch 경계 검토는 제공자 거부로 결과를 얻지 못했으며 승인으로 계산하지 않는다. 정확한 unmanaged 분류·OMP 경로 별칭·continue 최신 기록 선택은 직접 검토와 추가 회귀로 보완했다. 실제 호스트 시험에서는 원본 OMP 5개와 바이너리·zsh·Claude 설정을 보존했으며, 과거 원인 미확정 78 종료를 해결됐다고 바꾸지 않았다.
