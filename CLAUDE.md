# Ojak (AI Account Manager)

macOS 전용. Claude Code·Codex·omp 여러 구독 계정을 한 사용자 안에서 배정하는 로컬 서비스 + 메뉴바 앱. 제품명은 Ojak, 기술 식별자는 `aam` 계열(바이너리 `aam`/`aam-service`, Rust crate `aam-*`, Tauri crate `ai-account-manager`, LaunchAgent `ai.aam.service`, `AAM_HOME=~/Library/Application Support/AI Account Manager`). 부분적으로 이름을 바꾸지 말 것.

## 구조
- `crates/protocol` — 공유 타입, Unix socket RPC → [docs/domains/protocol.md](docs/domains/protocol.md)
- `crates/service` — `aam-service` 데몬 (RPC, SQLite, lease, 배정) → [service-core](docs/domains/service-core.md), omp 브릿지 → [omp-bridge](docs/domains/omp-bridge.md)
- `crates/adapters` — 공식 CLI 탐지·identity·안전 검사 → [adapters](docs/domains/adapters.md)
- `crates/launcher` — `aam` CLI + claude/codex shim + 설치 → [launcher](docs/domains/launcher.md)
- `integrations/omp` — omp 확장 2종 → [omp-bridge](docs/domains/omp-bridge.md), [omp-observer](docs/domains/omp-observer.md)
- `apps/desktop` — Tauri + React 앱 → [desktop](docs/domains/desktop.md)
- 설계: `docs/spec.md`, `docs/specs/*.md` · 도메인 색인: `docs/domains/INDEX.md`

## 명령
- 테스트: `~/.cargo/bin/cargo test` (default-members만; Tauri crate 제외) — `cargo`가 PATH에 없을 수 있음
- 타입 검사: `npm run typecheck`
- 앱 빌드: `npm run build` → `target/release/bundle/macos/Ojak.app`
- omp 확장 테스트: `node --test integrations/omp/aam-observer.test.mjs integrations/omp/aam-accounts.test.mjs`

## 반드시 지킬 것
- 인증을 우회하는 경로를 만들지 않는다. API key·base URL·auth helper가 보이면 `AUTH_OVERRIDE_CONFLICT`로 멈추고 자동으로 고치지 않는다.
- `integration.json`과 계정 `binary_path`에는 원본 CLI의 **진입 경로**(`~/.local/bin/claude` 등)를 저장하고 쓸 때 canonicalize한다. 버전별 경로를 고정하면 `claude install` 업데이트가 무시된다.
- shim이 원본으로 바로 넘기는 명령은 `adapters/src/lib.rs`의 `inspect_cli`/`self_update`에서만 정한다.
- lease는 불확실하면 슬롯을 유지한다. 타임아웃만으로 풀거나 spawn을 재시도하지 않는다.
- omp 공급자 ID 표(`ojak-*`, 이전 `aam-*`)는 `service/src/bridge.rs`, `integrations/omp/aam-accounts.js`, `launcher/src/omp_bridge.rs`, `apps/desktop/src/state.ts`(providerAliases·directProviders), `apps/desktop/src/ConnectionsView.tsx`(bridgeProviderOrder)를 함께 바꾼다.
- `logs/bridge.log` 줄 형식을 바꾸면 `apps/desktop/src-tauri/src/main.rs`의 파서도 고친다.
- 비밀(token, 이메일, 프롬프트)을 로그·오류·진단 내보내기에 넣지 않는다.
- 기능·버그 수정·UI/문구·인증·설치/업데이트 동작을 바꿔 사용자 설치본에 반영할 때는 **로컬 설치도 예외 없이 버전을 올린다**. 공개 버전과 같은 번호로 수정본을 설치하거나 배포하지 않는다. `node scripts/bump-version.mjs x.y.z`로 앱·CLI·서비스 버전을 함께 올리고 lockfile·CHANGELOG도 갱신한다. 기능 추가는 minor, 수정은 최소 patch다. Mac 반영·Windows 반영·공개 배포 여부를 따로 보고하고, 두 OS 검증 전 배포 완료라고 하지 않는다. 상세 게이트는 `docs/release-policy.md`를 따른다.

## 배포 (이 Mac)
설치본 `~/Applications/Ojak.app`을 교체한 뒤 `launchctl kickstart -k gui/$(id -u)/ai.aam.service` → `aam integration install` → 필요 시 `aam omp-bridge connect`. 서비스 재시작은 실행 중인 omp 브릿지 세션에 영향을 줄 수 있다.
