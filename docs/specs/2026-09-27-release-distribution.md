# 설치·배포·업데이트 계획 (GitHub 무료 공개)

상태: 릴리스 준비물은 공개 저장소 `lapalai/ojak` 한 곳 기준이다 (코드와 Releases를 같이 공개. 별도 `ojak-releases` 저장소는 쓰지 않음). 저장소 공개, 첫 태그, Homebrew tap, Apple 서명·공증은 아직.

구현된 것: `scripts/bump-version.mjs`, `.github/workflows/ci.yml`, `.github/workflows/release.yml` (`gh`로 업로드. sidecar 빌드가 `scripts/build.mjs`에 있어 `tauri-action`을 쓰지 않음), Tauri updater, `install.sh`, `README.md`, `LICENSE`.

저장소는 `lapalai/ojak`(2026-09-29 확정). 바꿀 때 README 표의 네 곳을 함께 고친다.

업데이터 공개키는 `tauri.conf.json`에 커밋된 실제 minisign 키다. `REPLACE_WITH_TAURI_UPDATER_PUBKEY`가 남아 있으면 앱은 업데이트 UI를 숨기고 확인 요청을 보내지 않는다. 개인 키는 저장소에 넣지 않는다. 첫 태그 전에 GitHub secret `TAURI_SIGNING_PRIVATE_KEY`와, 암호가 있으면 `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`를 넣는다. 비밀키는 저장소 밖에도 보관한다.

첫 실행 안내는 Gatekeeper 우회가 아니다. 공식 경로는 시스템 설정 → 개인정보 보호 및 보안 → 그래도 열기 (https://support.apple.com/102445). `install.sh`는 quarantine을 지우지 않는다. `--clear-quarantine`만 명시적 선택이다.

## 현재
- 빌드는 adhoc 서명, 공증 없음 → 받은 DMG는 Gatekeeper "확인되지 않은 개발자" 경고.
- 자동 업데이트 코드는 들어갔다. 커밋된 공개키는 자리표시자가 아니다. 비밀키가 설정된 릴리스부터 앱 안 업데이트가 검증된다.
- 코드는 `lapalai/ojak`에 있다. 공개 전환과 첫 태그는 아직이다. 별도 릴리스 저장소는 만들지 않는다.

## 설치
- GitHub Releases에 DMG. 첫 실행이 막히면 시스템 설정 → 개인정보 보호 및 보안 → 그래도 열기 (https://support.apple.com/102445). quarantine 삭제는 `install.sh --clear-quarantine` 명시 선택만.
- Homebrew tap(`brew install --cask <user>/tap/ojak`) 병행. 첫 실행 경고는 동일 [추정].
- 첫 실행 마법사: 서비스 설치 → shim → zsh PATH → omp 연결.
- 사용자가 늘면 Apple Developer($99/년) 서명·공증 추가.

## 배포
- 태그(`vX.Y.Z`) 푸시 → GitHub Actions macOS 러너(공개 저장소 무료): `npm run build` → DMG·업데이트 압축본·서명 → Release.
- 버전은 `apps/desktop/src-tauri/tauri.conf.json`과 `Cargo.toml` workspace 두 곳. 한 번에 올리는 스크립트.

## 업데이트
- Tauri updater + Release의 `latest.json`. 업데이트 서명은 Tauri 무료 키(Apple 서명과 별개).
- 시작 시·하루 1회 확인 → 설정·팝오버에 "업데이트 있음" → 버튼으로 설치.
- 교체 후 `launchctl kickstart -k gui/$UID/ai.aam.service`로 새 서비스 코드 적용.
- 서비스 재시작은 브릿지 omp 세션을 잠깐 끊음 → 실행 중 세션이 있으면 "지금 업데이트" / "나중에".
- 데이터 형식 이전(`integration.json` version, SQLite `user_version`)은 서비스 시작 시.
- 확인 필요: adhoc 서명 앱은 업데이트마다 서명이 바뀌어 TCC 권한이 초기화될 수 있음 [추정].

## 공개 전 필수
- 개인정보 제거: 실제 이메일(문서 예시, `docs/SCREEN_FLOWS.md`, `artifacts/`, 제작자 연락처), 절대 경로(`/Users/...`). `artifacts/`·개인 스크린샷 제외.
- LICENSE(MIT), README, `.gitignore` 점검. `docs/spec.md`의 개인 환경 정보 정리.
- 기록에 남은 개인정보는 지우기 어렵다. 공개 전에 워킹 트리의 개인정보를 정리한다.

## 순서
1. 개인정보 검사·정리 (진행 중). 첫 커밋은 이미 있다.
2. 버전 스크립트 + Actions 릴리스 워크플로 (구현)
3. Tauri updater + 서비스 재시작 처리 (구현)
4. README(설치·첫 실행·제거) (구현). Homebrew tap은 아직
5. `lapalai/ojak` 공개와 첫 태그 (아직). 별도 `ojak-releases` 저장소는 만들지 않는다.

## 미정 (사용자 답 필요)
- GitHub 저장소: `lapalai/ojak`로 확정(2026-09-29).
- 제작자 연락처 공개 여부

## 공개·홍보 문구 원칙 (2026-09-29)

바이럴 공개 시 약관 리스크를 키우지 않도록 소개 문구를 다음 범위로 제한한다.

- 쓴다: "여러 구독 계정의 사용량을 한눈에", "계정별로 공식 CLI 실행", "본인 계정 전용", "로컬에서만 동작".
- 쓰지 않는다: "한도 우회", "무제한", "계정 돌려쓰기/로테이션", "rate limit 회피", 공유 계정·팀 공용 사용 암시.
- omp 브릿지는 선택 기능으로만 언급하고, 언급할 때는 공급자 약관 고지 링크를 함께 둔다. 데모·스크린샷의 주인공으로 쓰지 않는다.
- 스크린샷·GIF는 앱의 개인정보 숨기기를 켠 상태로 찍는다.
- 공개 전 남은 일: Apple 서명·공증(보류), 스크린샷·GIF(보류), 히스토리·Actions 로그 개인정보 재검사.
