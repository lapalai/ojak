# 릴리스 정책

만드는 방법(빌드·서명·태그)은 `README.md`와 `.github/workflows/release.yml`에 있다. 이 문서는 언제, 무엇을, 어떤 순서로 내는지 정한다.

## 버전
- 형식은 `x.y.z`만 쓴다(`scripts/bump-version.mjs`가 강제). 태그는 `v` + 같은 버전이다.
- **patch(z):** 버그 수정, 문구·화면 다듬기.
- **minor(y):** 사용자에게 보이는 기능 추가·변경.
- **1.0 전 예외:** 서비스 RPC 프로토콜, DB 형식(`user_version`), `integration.json` 형식이 바뀌어도 minor로 올린다. 대신 릴리스 노트에 "서비스 재시작 필요" 또는 "이전 버전으로 되돌릴 수 없음"을 반드시 적는다.
- 같은 버전 번호로 두 번 빌드해 배포하지 않는다. 앱 업데이트는 더 높은 버전만 받는다.

## 변경 기록
- `CHANGELOG.md`의 `## 다음 버전`에 변경할 때마다 한 줄씩 쌓는다. 사용자 관점 문장으로 쓰고, 내부 리팩터링은 적지 않는다.
- 릴리스 때 그 절을 `## x.y.z (YYYY-MM-DD)`로 바꾸고, 같은 내용을 annotated 태그 메시지로 쓴다. 워크플로가 태그 메시지를 릴리스 노트와 앱 업데이트 알림에 그대로 쓴다.

## 태그 전 확인
1. 작업 트리가 깨끗하고 main에 푸시되어 있다.
2. `~/.cargo/bin/cargo test`, `npm run typecheck`, `npm run test:desktop`, `node --test integrations/omp/aam-accounts.test.mjs integrations/omp/aam-observer.test.mjs` 통과.
3. `node scripts/bump-version.mjs x.y.z` 후 커밋.
4. 실기기 확인: macOS와 Windows에서 **이전 릴리스 설치본 → 앱 안 업데이트**로 새 버전이 설치되고, 서비스 재시작 뒤 `aam setup --check`가 `ready: true`인지 본다. Windows는 일반 권한 세션에서 확인한다(관리자 SSH로 설치하면 기록 소유자가 달라진다).
5. `git tag -a vx.y.z -F <변경 기록 절>` → `git push origin vx.y.z`.

## 배포 위치
- 코드와 릴리스는 공개 저장소 `lapalai/ojak` 한 곳이다. 별도 `ojak-releases` 저장소는 쓰지 않는다.
- 앱은 `plugins.updater.endpoints`의 `latest.json`을 인증 없이 받는다. 주소는 `https://github.com/lapalai/ojak/releases/latest/download/latest.json`이다.
- `v*` 태그를 이 저장소에 푸시하면 `.github/workflows/release.yml`이 macOS DMG, Windows setup.exe, 업데이터 서명, `latest.json`, `SHA256SUMS`를 같은 저장소 Releases에 올린다. 워크플로는 `pull_request`/`pull_request_target`이 없고, `github.repository`가 `lapalai/ojak`인 태그 푸시에서만 돈다. 포크 pull request에는 서명 비밀키가 전달되지 않는다.

## 공개 전 수동 단계
1. `plugins.updater.pubkey`는 이미 실제 minisign 공개키다. 그와 한 쌍인 비밀키 내용만 GitHub Actions secret `TAURI_SIGNING_PRIVATE_KEY`에 넣는다. 암호가 있으면 `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`도 넣는다. 비밀키는 저장소·로그·진단에 넣지 말고, 저장소 밖에도 보관한다. 잃으면 이미 설치된 앱은 앱 안 업데이트를 검증할 수 없다. 첫 태그 전에 비밀키를 잃었다면 새 쌍을 만들고 공개키를 바꾼 뒤에만 태그를 푸시한다.
2. 첫 태그는 위 비밀키가 들어간 뒤에 `git tag -a vx.y.z` → `git push origin vx.y.z`다. 워크플로는 빌드 전에 `scripts/verify-signing-key.mjs`로 비밀키가 커밋된 공개키의 짝인지(서명→검증) 실제로 확인한다. 짝이 다르거나 암호가 틀리거나 공개키가 자리표시자면 멈춘다.
3. 키 교체(2026-10-07): 이전 키(id `8C4593A2DEE40EF`)는 검증 중 로컬 로그에 노출돼 폐기했다. 새 키 id는 `D9AFF69B91D5CB2F`이며 암호가 있다. 이전 키로 서명된 릴리스는 발행된 적이 없다.
4. 이 빌드는 Apple Developer ID 서명·공증과 Windows Authenticode 서명이 없다. 첫 실행에서 Gatekeeper는 "확인되지 않은 개발자", SmartScreen은 "Windows의 PC 보호"를 띄운다. 사용자 안내는 README 설치 절과 같다. 서명·공증은 나중 일이다.

## 되돌리기
- 이미 올린 태그·릴리스는 지우거나 덮어쓰지 않는다.
- 문제 버전은 릴리스 노트 맨 위에 문제를 적고, 이전 코드로 patch를 하나 올려 다시 낸다(예: 0.2.0 문제 → 0.1.x 코드로 0.2.1).
- 되돌릴 수 없는 형식 변경이 들어간 버전 뒤에는 고친 코드를 앞으로 내는 방법만 쓴다.

## 채널
- 지금은 정식 하나만 둔다. 베타가 필요해지면 별도 endpoint로 나누고, 그때 버전 형식 규칙을 함께 바꾼다.

## 업데이트 서명 키
- 비밀키는 GitHub secret(`TAURI_SIGNING_PRIVATE_KEY`, `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`)에만 둔다. 공개키와 한 쌍이어야 하며, 분실에 대비해 저장소 밖에도 보관한다.
