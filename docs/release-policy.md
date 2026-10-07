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
5. `git tag -a vx.y.z -F <변경 기록 절>` → `git push origin vx.y.z`. 태그는 main에 이미 푸시된 커밋에 만들고, 반드시 annotated로 만든다. 태그를 만들 수 있는 사람은 저장소 Admin뿐이다(아래 "태그 보호").
6. 릴리스가 올라온 뒤 이 Mac의 설치본을 교체했다면 서비스를 다시 시작하고 실행 중인 서비스 버전을 확인한다. `~/Applications/Ojak.app`을 교체 → `launchctl kickstart -k gui/$(id -u)/ai.aam.service` → `aam integration install`(필요 시 `aam omp-bridge connect`). 재시작은 실행 중인 omp 브릿지 세션을 잠깐 끊으니, 세션이 있으면 끝난 뒤 한다. 그 뒤 `ps -axo command | grep '[a]am-service'`로 뜬 경로가 새 앱 안의 `aam-service`인지, `~/Applications/Ojak.app/Contents/MacOS/aam-service --version`이 새 버전인지 본다. 새 파일로 교체해도 이미 떠 있던 프로세스는 이전 코드로 남는다.

## 배포 위치
- 코드와 릴리스는 공개 저장소 `lapalai/ojak` 한 곳이다. 별도 `ojak-releases` 저장소는 쓰지 않는다.
- 앱은 `plugins.updater.endpoints`의 `latest.json`을 인증 없이 받는다. 주소는 `https://github.com/lapalai/ojak/releases/latest/download/latest.json`이다.
- `v*` 태그를 이 저장소에 푸시하면 `.github/workflows/release.yml`이 macOS DMG, Windows setup.exe, 업데이터 서명, `latest.json`, `SHA256SUMS`를 같은 저장소 Releases에 올린다. 워크플로는 `pull_request`/`pull_request_target`이 없고, `github.repository`가 `lapalai/ojak`인 태그 푸시에서만 돈다. 포크 pull request에는 서명 비밀키가 전달되지 않는다. 태그가 annotated이고 main에 있는 커밋을 가리키는지도 서명 전에 검사한다("태그 보호").

## 공개 전 수동 단계
1. `plugins.updater.pubkey`는 이미 실제 minisign 공개키다. 그와 한 쌍인 비밀키 내용만 GitHub Actions secret `TAURI_SIGNING_PRIVATE_KEY`에 넣는다. 암호가 있으면 `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`도 넣는다. 비밀키는 저장소·로그·진단에 넣지 말고, 저장소 밖에도 보관한다. 잃으면 이미 설치된 앱은 앱 안 업데이트를 검증할 수 없다. 첫 태그 전에 비밀키를 잃었다면 새 쌍을 만들고 공개키를 바꾼 뒤에만 태그를 푸시한다.
2. 첫 태그는 위 비밀키가 들어간 뒤에 `git tag -a vx.y.z` → `git push origin vx.y.z`다. 워크플로는 빌드 전에 `scripts/verify-signing-key.mjs`로 비밀키가 커밋된 공개키의 짝인지(서명→검증) 실제로 확인한다. 짝이 다르거나 암호가 틀리거나 공개키가 자리표시자면 멈춘다.
3. 키 교체(2026-10-07): 이전 키(id `8C4593A2DEE40EF`)는 검증 중 로컬 로그에 노출돼 폐기했다. 새 키 id는 `D9AFF69B91D5CB2F`이며 암호가 있다. 이전 키로 서명된 릴리스는 발행된 적이 없다.
4. 이 빌드는 Apple Developer ID 서명·공증과 Windows Authenticode 서명이 없다. 첫 실행에서 Gatekeeper는 "확인되지 않은 개발자", SmartScreen은 "Windows의 PC 보호"를 띄운다. 사용자 안내는 README 설치 절과 같다. 서명·공증은 나중 일이다.

## 태그 보호
- 이미 올린 태그는 지우거나 옮기거나 덮어쓰지 않는다. 예외는 없다.
- v0.1.1은 커밋 `73bfb2a`로 고정이다. 처음 `d2389b9`에 달았던 v0.1.1 태그는 첫 실행이 게시 전에 실패했다는 이유로 다시 달았는데, 그것은 정책 위반이었고 다시 일어나서는 안 된다. 이후 실패는 아래 "게시 전에 릴리스가 실패했을 때" 절차만 쓴다.
- 저장소 ruleset `release-tags`(id `24619490`)가 `refs/tags/v*`에 걸려 있다. 규칙은 `creation`(생성 제한), `update`, `deletion`, `non_fast_forward`이고 우회는 저장소 역할 Admin(actor_id 5)만 가능하다. 그래서 쓰기 권한만 있는 사람은 `v*` 태그를 만들지도 고치지도 지우지도 못한다. 확인은 `gh api repos/lapalai/ojak/rulesets`. 이 ruleset을 끄거나 우회 대상을 넓히지 않는다.
- Admin도 `update`·`deletion`을 우회할 수 있지만 정책상 쓰지 않는다. ruleset은 실수와 권한 없는 푸시를 막는 장치이고, 정책을 대신하지 않는다.
- `release.yml`은 빌드·게시 두 job 모두 서명 비밀키를 쓰기 전에 다음을 검사하고, 하나라도 어긋나면 멈춘다: ① 저장소가 정확히 `lapalai/ojak`, ② 태그가 annotated(`git cat-file -t`가 `tag`), ③ 태그가 가리키는 커밋이 푸시된 커밋(`GITHUB_SHA`)과 같음, ④ 그 커밋이 `origin/main`의 조상(`git merge-base --is-ancestor`). 다른 브랜치에서 만든 태그는 서명 릴리스가 되지 못한다.
- 이 검사는 태그가 가리키는 커밋의 워크플로 파일로 돈다. 그래서 검사만으로는 충분하지 않고 위 ruleset(Admin만 태그 생성)과 함께 써야 한다. 워크플로 파일을 고칠 때 이 두 검사를 지우지 않는다.

## 게시 전에 릴리스가 실패했을 때
빌드·테스트·서명 확인 등으로 워크플로가 실패하고 GitHub Release가 만들어지지 않았어도 **태그는 건드리지 않는다.** 실패한 태그는 기록으로 남는다.
1. 원인을 main에서 고친다. 브랜치 → PR → 병합으로 하고, 태그가 있던 커밋을 되돌리거나 태그를 옮기려 하지 않는다.
2. `CHANGELOG.md`의 `## 다음 버전`에 쌓아 둔 내용을 그대로 두고, `node scripts/bump-version.mjs`로 **다음 patch**를 올린다(예: v0.2.0 실패 → 0.2.1). 실패한 버전 번호를 다시 쓰지 않는다.
3. "태그 전 확인"을 처음부터 다시 거쳐 새 annotated 태그를 만든다. 릴리스 노트에는 새 버전의 변경만 적는다.
4. 실패한 run은 Actions에 그대로 둔다. 일부 산출물이 올라갔거나 Release가 초안으로 남았다면 지우지 말고 먼저 Admin에게 상황을 공유한다.

## 되돌리기
- 이미 올린 태그·릴리스는 지우거나 덮어쓰지 않는다.
- 문제 버전은 릴리스 노트 맨 위에 문제를 적고, 이전 코드로 patch를 하나 올려 다시 낸다(예: 0.2.0 문제 → 0.1.x 코드로 0.2.1).
- 되돌릴 수 없는 형식 변경이 들어간 버전 뒤에는 고친 코드를 앞으로 내는 방법만 쓴다.

## 채널
- 지금은 정식 하나만 둔다. 베타가 필요해지면 별도 endpoint로 나누고, 그때 버전 형식 규칙을 함께 바꾼다.

## 업데이트 서명 키
- 비밀키는 GitHub secret(`TAURI_SIGNING_PRIVATE_KEY`, `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`)에만 둔다. 공개키와 한 쌍이어야 하며, 분실에 대비해 저장소 밖에도 보관한다.
