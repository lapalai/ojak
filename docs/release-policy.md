# 릴리스 정책

빌드 준비는 [기여 안내](../CONTRIBUTING.md), 설치·사용은 [README](../README.md)에 있다. 이 문서는 버전·서명·태그·공개 순서를 정한다. 실제 워크플로는 [release.yml](../.github/workflows/release.yml)을 따른다.

## 버전
- 형식은 `x.y.z`만 쓴다(`scripts/bump-version.mjs`가 강제). 태그는 `v` + 같은 버전이다.
- **patch(z):** 버그 수정, 문구·화면 다듬기.
- **minor(y):** 사용자에게 보이는 기능 추가·변경.
- **1.0 전 예외:** 서비스 RPC 프로토콜, DB 형식(`user_version`), `integration.json` 형식이 바뀌어도 minor로 올린다. 대신 릴리스 노트에 "서비스 재시작 필요" 또는 "이전 버전으로 되돌릴 수 없음"을 반드시 적는다.
- 기능·버그 수정·UI/문구·인증·설치/업데이트 변경을 사용자 설치본에 반영하는 작업은 **반드시 버전을 올린다**. 공개 릴리스뿐 아니라 개발자가 사용자 PC의 앱을 직접 교체하는 로컬 설치도 포함한다. 문서만 수정하고 실행 산출물은 바꾸지 않는 작업은 제외한다.
- 같은 번호로 내용이 다른 실행 산출물을 설치·배포하지 않는다. 이미 설치했거나 태그를 만든 후보에 추가 수정이 생기면 다음 patch를 사용한다. 검증된 동일 바이트의 후보를 다른 PC에 설치하거나 공개하는 것은 재사용 가능하다.
- `node scripts/bump-version.mjs x.y.z`로 Tauri·Rust workspace·npm 버전을 함께 올리고 Cargo.lock·package-lock.json·CHANGELOG를 갱신한다. 기능 추가는 minor, 수정은 최소 patch이며 현재 설치·공개된 버전보다 높아야 한다. 앱 업데이트는 더 높은 버전만 받는다.
- 완료 보고에는 새 버전, Mac 설치 여부, Windows 설치 여부, 공개 릴리스 여부를 분리한다. 한쪽 OS의 로컬 적용을 전체 배포라고 부르지 않는다.

## 변경 기록
- `CHANGELOG.md`의 `## 다음 버전`에 변경할 때마다 한 줄씩 쌓는다. 사용자 관점 문장으로 쓰고, 내부 리팩터링은 적지 않는다.
- 릴리스 때 그 절을 `## x.y.z (YYYY-MM-DD)`로 바꾸고, 같은 내용을 annotated 태그 메시지로 쓴다. 워크플로가 태그 메시지를 릴리스 노트와 앱 업데이트 알림에 그대로 쓴다.

## 태그 전 확인
1. 작업 트리가 깨끗하고 main에 푸시되어 있다.
2. `~/.cargo/bin/cargo test`, `npm run typecheck`, `npm run test:desktop`, `node --test integrations/omp/aam-accounts.test.mjs integrations/omp/aam-observer.test.mjs` 통과.
3. `node scripts/bump-version.mjs x.y.z` 후 커밋.
4. 실기기 설치 검증 결과와 공개 후 앱 안 업데이트 검증 계획을 준비한다. 서명 산출물은 태그 뒤에 생기므로 이 단계에서 서명 업데이트 통과를 주장하지 않는다.
5. `git tag -a vx.y.z -F <변경 기록 절>` → `git push origin vx.y.z`. 태그는 main에 이미 푸시된 커밋에 만들고, 반드시 annotated로 만든다. 태그를 만들 수 있는 사람은 저장소 Admin뿐이다(아래 "태그 보호"). 워크플로는 서명된 **비공개 draft**까지만 만든다. 아래 공개 전 검증을 통과하기 전에는 게시하지 않는다.
6. 릴리스가 올라온 뒤 이 Mac의 설치본을 교체했다면 서비스를 다시 시작하고 실행 중인 서비스 버전을 확인한다. `~/Applications/Ojak.app`을 교체 → `launchctl kickstart -k gui/$(id -u)/ai.aam.service` → `aam integration install`(필요 시 `aam omp-bridge connect`). 재시작은 실행 중인 omp 브릿지 세션을 잠깐 끊으니, 세션이 있으면 끝난 뒤 한다. 그 뒤 `ps -axo command | grep '[a]am-service'`로 뜬 경로가 새 앱 안의 `aam-service`인지, `~/Applications/Ojak.app/Contents/MacOS/aam-service --version`이 새 버전인지 본다. 새 파일로 교체해도 이미 떠 있던 프로세스는 이전 코드로 남는다.

## 배포 위치
- 코드와 릴리스는 공개 저장소 `lapalai/ojak` 한 곳이다. 별도 `ojak-releases` 저장소는 쓰지 않는다.
- 앱은 `plugins.updater.endpoints`의 `latest.json`을 인증 없이 받는다. 주소는 `https://github.com/lapalai/ojak/releases/latest/download/latest.json`이다.
- `v*` 태그를 이 저장소에 푸시하면 `.github/workflows/release.yml`이 macOS DMG, Windows setup.exe, 업데이터 서명, `latest.json`, `SHA256SUMS`를 같은 저장소의 **비공개 draft Release**에 올린다. 정식 최신 버전은 바뀌지 않는다. 워크플로는 `pull_request`/`pull_request_target`이 없고, `github.repository`가 `lapalai/ojak`인 태그 푸시에서만 돈다. 포크 pull request에는 서명 비밀키가 전달되지 않는다. 태그가 annotated이고 main에 있는 커밋을 가리키는지도 서명 전에 검사한다("태그 보호").
- 같은 draft에는 native Mac arm64·Windows x64 호스트에서 빌드하고 `--version`을 실행한 `omp-darwin-arm64`, `omp-windows-x64.exe`와 `omp-compatible.json`도 포함한다. `scripts/package-compatible-omp.mjs build <준비한 omp 소스> <stage>`가 pinned 버전을 대조하고, 두 OS 산출물을 모은 뒤 `manifest <stage>`가 실제 바이트의 크기·SHA-256을 기록한다. 세 파일 모두 `SHA256SUMS`에 포함한다. 어느 OS 빌드라도 없으면 draft 생성을 중단한다.
- omp `18.12.0-ojak.2`부터 일반 `omp update`·`--check`·`--force`·`--stable` 및 시작 시 업데이트 안내는 공개 stable `lapalai/ojak` 최신 Release만 조회한다. 같은 Release의 manifest는 `schemaVersion: 1`, `admissionProtocol: 1`, Ojak fork `version`, `platforms.darwin-arm64`·`platforms.win32-x64`의 `{asset,sha256,size}`를 담는다. draft·prerelease·누락/불일치 manifest·미지원 플랫폼·네트워크 오류에서는 기존 실행 파일을 보존한다. `--canary`는 명시적으로 거절하며 upstream/npm/build-service로 우회하지 않는다. 기존 채널 설정과 인증 설정은 바꾸지 않고 플러그인 업데이트는 기존 경로를 유지한다.
- 이 호환 manifest는 GitHub 공개 Release 신뢰 경계 안의 무결성 계약이며 별도 코드 서명을 대신하지 않는다. 공개 전에는 두 OS에서 실제 업데이트·실패 시 rollback과 Orca의 명시적 omp 버전 계약도 확인한다. 소스 변경이나 draft 생성만으로 설치·공개 완료라고 기록하지 않는다.

## 공개 전 수동 단계
1. `plugins.updater.pubkey`는 이미 실제 minisign 공개키다. 그와 한 쌍인 비밀키 내용만 GitHub Actions secret `TAURI_SIGNING_PRIVATE_KEY`에 넣는다. 암호가 있으면 `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`도 넣는다. 비밀키는 저장소·로그·진단에 넣지 말고, 저장소 밖에도 보관한다. 잃으면 이미 설치된 앱은 앱 안 업데이트를 검증할 수 없다. 첫 태그 전에 비밀키를 잃었다면 새 쌍을 만들고 공개키를 바꾼 뒤에만 태그를 푸시한다.
2. 첫 태그는 위 비밀키가 들어간 뒤에 `git tag -a vx.y.z` → `git push origin vx.y.z`다. 워크플로는 빌드 전에 `scripts/verify-signing-key.mjs`로 비밀키가 커밋된 공개키의 짝인지(서명→검증) 실제로 확인한다. 짝이 다르거나 암호가 틀리거나 공개키가 자리표시자면 멈춘다.
3. 키 교체(2026-10-07): 이전 키(id `8C4593A2DEE40EF`)는 검증 중 로컬 로그에 노출돼 폐기했다. 새 키 id는 `D9AFF69B91D5CB2F`이며 암호가 있다. 이전 키로 서명된 릴리스는 발행된 적이 없다.
4. 이 빌드는 Apple Developer ID 서명·공증과 Windows Authenticode 서명이 없다. 첫 실행에서 Gatekeeper는 "확인되지 않은 개발자", SmartScreen은 "Windows의 PC 보호"를 띄운다. 사용자 안내는 README 설치 절과 같다. 서명·공증은 나중 일이다.

### 서명 후보 검증과 공개 승인

- Admin은 draft의 서명된 산출물을 받아 기존 공개키로 서명을 검증하고, 그 **동일한 파일**을 macOS·Windows의 이전 설치본에 적용한다. 설치 후 서비스 버전과 `aam setup --check`의 `ready: true`를 확인한다. Windows는 일반 사용자 권한에서 설치한다.
- **0.3.0 전환 예외(사용자 승인):** 0.2.0은 공개된 최신 endpoint만 확인하므로 비공개 후보를 앱 안에서 선택할 수 없다. 이번 공개 전 검증은 서명된 설치 파일을 직접 적용하는 방식으로 한다. 이를 ‘앱 안 업데이트 통과’로 기록하지 않는다.
- **0.10.1 설치 보류 예외(사용자 승인, 2026-10-15):** 사용자가 공개 배포만 요청하고 기존 앱은 나중에 직접 반영하기로 했다. 이번 후보는 Mac·Windows에서 기존 설치본을 교체하거나 서비스를 재시작하지 않고, 별도 데이터 폴더의 실제 서비스·CLI와 서명 산출물 추출본으로 검증한다. 기존 설치본 업그레이드·앱 안 업데이트·실계정 OAuth 갱신 검증을 통과했다고 기록하지 않는다. 산출물 서명·체크섬 확인과 동일 바이트 공개 원칙은 유지하며, 이 예외를 이후 릴리스의 일반 게이트로 확대하지 않는다.
- 두 OS의 이전/새 버전, 산출물 SHA-256, 서명 검증 결과, 설치 권한, 서비스 버전, setup 결과를 릴리스 검증 기록으로 남긴다. 실패하면 draft를 보존하고 공개하지 않는다. 태그나 산출물을 교체해서 재사용하지 않는다.
- 검증을 통과한 draft만 Admin이 `gh release edit vx.y.z --repo lapalai/ojak --draft=false --latest`로 공개한다. 다시 빌드하거나 파일을 바꾸지 않는다.
- 공개 후에는 **이전 공개 릴리스 → 앱 안 업데이트** 경로를 두 OS에서 별도로 확인하고 결과를 기록한다. 직접 설치 검증과 앱 안 업데이트 검증은 구분한다. 실패하면 “되돌리기” 절차에 따라 다음 patch로 고친다.

## 태그 보호
- 이미 올린 태그는 지우거나 옮기거나 덮어쓰지 않는다. 예외는 없다.
- v0.1.1은 커밋 `73bfb2a`로 고정이다. 처음 `d2389b9`에 달았던 v0.1.1 태그는 첫 실행이 게시 전에 실패했다는 이유로 다시 달았는데, 그것은 정책 위반이었고 다시 일어나서는 안 된다. 이후 실패는 아래 "게시 전에 릴리스가 실패했을 때" 절차만 쓴다.
- 저장소 ruleset `release-tags`(id `24619490`)가 `refs/tags/v*`에 걸려 있다. 규칙은 `creation`(생성 제한), `update`, `deletion`, `non_fast_forward`이고 우회는 저장소 역할 Admin(actor_id 5)만 가능하다. 그래서 쓰기 권한만 있는 사람은 `v*` 태그를 만들지도 고치지도 지우지도 못한다. 확인은 `gh api repos/lapalai/ojak/rulesets`. 이 ruleset을 끄거나 우회 대상을 넓히지 않는다.
- Admin도 `update`·`deletion`을 우회할 수 있지만 정책상 쓰지 않는다. ruleset은 실수와 권한 없는 푸시를 막는 장치이고, 정책을 대신하지 않는다.
- `release.yml`은 build·draft 두 job 모두 게시나 서명 비밀키 사용 전에 다음을 검사하고, 하나라도 어긋나면 멈춘다: ① 저장소가 정확히 `lapalai/ojak`, ② 태그가 annotated(`git cat-file -t`가 `tag`), ③ 태그가 가리키는 커밋이 푸시된 커밋(`GITHUB_SHA`)과 같음, ④ 그 커밋이 `origin/main`의 조상(`git merge-base --is-ancestor`). 다른 브랜치에서 만든 태그는 서명 릴리스가 되지 못한다.
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

업데이터 서명은 Apple Developer ID 서명·공증 및 Windows Authenticode와 별개다. 키를 새로 만들어야 한다면 저장소 밖 경로를 지정한다.

```sh
npx tauri signer generate -w "$HOME/.tauri/ojak.key"
```

1. 출력된 공개키를 `apps/desktop/src-tauri/tauri.conf.json`의 `plugins.updater.pubkey`에 넣는다. 현재 값은 실제 minisign 공개키다. 빈 값이나 `REPLACE_WITH_TAURI_UPDATER_PUBKEY`로 바꾸면 업데이트 UI와 확인 요청이 비활성화된다.
2. 짝이 되는 비밀키 내용을 `TAURI_SIGNING_PRIVATE_KEY`에, 암호가 있으면 `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`에 넣는다. 비밀키를 커밋하지 않는다.
3. 첫 태그 전에 키 쌍을 확인한다. `scripts/verify-signing-key.mjs`는 작은 파일에 서명한 뒤 저장소의 공개키로 검증한다. 짝이 다르거나 암호가 틀리거나 값이 비어 있으면 중단한다. 비밀 값이 명령 이력이나 로그에 노출되지 않는 방식으로 환경변수를 공급한다.
4. 키를 바꾸면 기존 설치본이 새 서명을 검증하지 못할 수 있다. 새 버전의 직접 설치 안내와 함께 배포하며, 기존 키로 서명한 것처럼 취급하지 않는다.

비밀키가 없는 로컬 빌드에서 `scripts/build.mjs`는 업데이터 산출물만 끄고 앱·설치 파일을 만든다. 키가 있으면 Mac의 `Ojak.app.tar.gz`와 `.sig` 등 업데이터 산출물도 만든다. 빌드 완료와 두 OS 배포 검증 완료는 구분한다.

브랜치·PR CI는 `node scripts/build.mjs --verify-build`로 Windows NSIS·Mac DMG 전체 빌드를 검증하고 `verification-only-*` Actions artifact에 7일 보관한다. 서명 키를 받지 않으며 업데이터 산출물·태그·Release를 만들지 않는다. 태그나 다른 이벤트에서 이 옵션을 사용하면 거절한다. 이 빌드 성공은 설치·서명·공개 승인 검증을 대신하지 않는다.

`Client verification`은 고정된 omp·Orca 수정본을 별도 Mac runner에서 빌드한다. omp 실행 파일의 실제 버전 출력, Orca 전체 Mac 설치 파일(9개 원격 대상 포함), 격리 HOME의 실제 원격 터미널 왕복을 검사한다. 결과는 `verification-only-mac-clients`에만 보관하며 upstream이나 Ojak Release로 게시하지 않는다. 공식 네이티브 입력은 고정 SHA-256과 원본 템플릿 봉인 검증을 모두 통과해야 가져온다. 작업 실행·성공 여부는 각 Actions run의 실제 결과로 확인한다.

## 0.16.2 호환 omp 업데이트 복구 (2026-10-19)

- Mac의 공식 omp 18.12.2를 백업하고 검증된 18.12.0-ojak.1로 먼저 복구했다. 이어 호환 업데이트를 구현한 새 18.12.0-ojak.2를 빌드해 설치했다. 기존 omp·Orca 프로세스는 종료하지 않았으며 실행 중인 일반판 세션은 저장 후 재시작해야 새 실행본을 사용한다.
- 새 Mac omp SHA-256은 `de6d393a0d01d065e2abf51f39b6c8e003b5036b4a4e86140d6e43513f963914`이다. 실제 주 요청의 `OJAK_COMPATIBLE_UPDATE_OK`, admission queued → submitted → completed 및 advisor의 정상 stop을 확인했다.
- 업데이트 관련 테스트 120개 통과·1개 skip, omp 타입 검사, 패키징 테스트 3개, launcher omp_bridge 검사 6개가 통과했다. 깨끗한 pinned checkout에서 두 fork 패치 적용을 확인했다. 기존 일반판 설치 명령 문자열만 고정한 테스트 2개는 삭제했다.
- 격리된 실제 omp 실행 파일을 새 버전으로 교체하고 `--version` 실행까지 확인했다. manifest가 주장하는 버전과 실제 실행 파일 버전이 다를 때 원래 실행 파일 해시로 원복됨을 확인했다. 이 검증의 릴리스 전송은 주입된 fixture이며 공개 다운로드 성공을 뜻하지 않는다.
- 실제 GitHub 공개 endpoint를 사용하는 새 실행본의 `update`, `--check`, `--force`는 호환 manifest가 아직 없는 현재 릴리스를 명시적으로 거절했고 실행 파일 해시가 유지됐다. `--canary`도 거절한다. 공개 배포 전까지 업데이트 보류 메시지는 정상이며 upstream 일반판으로 대체하지 않는다.
- Ojak 0.16.2 앱/DMG를 빌드하고 ad-hoc codesign 검증을 통과했다. 관리 ACTIVE·ORPHANED 세션을 확인해 Ojak 앱 설치본 0.16.0 및 실행 서비스를 교체·재시작하지 않았다. 기존 앱의 연결/확장 갱신은 이전 omp 버전을 요구할 수 있으므로 관리 작업 종료 후 0.16.2 앱을 함께 반영해야 한다.
- Windows 빌드·설치, 새 Orca 설치, 공개 릴리스 게시, 서명된 앱 업데이트는 수행하지 않았다. 공개 시에는 새 호환 클라이언트와 manifest를 동일 draft에 모으고 기존 양 OS 검증 게이트를 통과해야 한다.

## 0.16 Windows 반영 및 공개 후보 진행 (2026-10-18)

- Windows PC의 기존 0.10.1에서 0.16.0 설치 파일을 빌드했다. Rust 431개 통과(서브프로세스 전용 helper 1개는 부모 테스트에서 실행), desktop 102개·omp 확장 25개·타입 검사 및 NSIS 빌드가 통과했다. 소스 commit은 `4183eaf`, setup SHA-256은 `401c44257760cd78c2d78befb8f1c05e51f322d8e48145e4ca1c10be0b5577f1`이다.
- 사용자는 “ojak만 종료하고 업데이트”를 명시 선택했다. 모델 요청·인증 갱신이 없는 시점에 같은 사용자·Interactive/Limited Task로 Ojak만 업데이트했다. 최초 설치는 `INSTALLER_FILES_BUSY`로 중단됐으며, 잠금을 잡고 있던 같은 설치 경로의 `aam.exe omp-observer` 보조 프로세스만 정리한 뒤 NSIS prepare·finish가 성공했다. omp·Orca 프로세스 및 실행 파일은 종료·교체하지 않았다.
- PowerShell `Start-Process -Wait`가 설치 완료 뒤에도 자식 서비스 종료를 기다려 모니터만 남았다. 설치 완료는 installer log의 prepare/finish OK, 실제 CLI·실행 서비스·감독 프로세스 0.16.0, `setup --check`의 ready true·serviceVersionMismatch false로 별도 확인했다. 설치 Task 등록을 정리했고 기존 omp·Orca의 확인한 4개 PID가 유지됐음을 확인했다. 네이티브 Windows 화면의 시각 검증은 하지 않았다.
- Windows proof는 사용자 홈의 `ojak-016-verified-install.json`이다. `setup --status`의 미검증 ready false와 실제 `setup --check`의 ready true를 구분해서 기록했다.
- 공개용 서명 빌드는 설치한 로컬 0.16.0과 다른 산출물이므로 0.16.1로 분리했다. PR #43 (`release/0.16.0`)의 제품 코드 commit `0e059dd`는 CI run `39086044435`에서 macOS·Windows의 테스트·타입 검사·전체 unsigned 설치 파일 빌드를 통과했다. 필수 reviewer `antoniopark0-sys`에게 승인 요청했으며, 승인 전 main 병합·태그·서명 draft·공개를 진행하지 않는다.
- 사용자가 처음 보고한 `BRIDGE_ADMISSION_STOPPED` 문구는 확장의 durableNativeStream 사전 검사에서 requestKind/sessionId/liveContext/persistSession 중 하나라도 확인되지 않을 때 발생했다. 당시 실행본은 Mac omp 18.12.1, Windows omp 18.11.3으로 필수 수정본과 달랐다. Windows의 추가 `BRIDGE_CLIENT_UPGRADE_REQUIRED` 오류는 구형 확장이 `X-Ojak-Admission`을 보내지 않아 서버에서 거절한 경우였다. 호환 검사·인증 설정을 우회하지 않고 아래 복구를 수행했다.
- 현재 Mac·Windows 로컬 설치는 0.16.0, 공개 최신은 0.13.2다. 공개 후보 0.16.1의 서명된 동일 산출물 양 OS 적용과 앱 안 업데이트 검증·공개는 아직 완료하지 않았다.

### 로컬 omp 연동 복구

- 사용자의 추가 복구 요청으로 공개 v0.13.2의 동일 omp `18.12.0-ojak.1` 바이너리를 적용했다. GitHub asset digest와 실제 SHA-256을 대조했다(Mac `8aafda40a5371da9c8249c3319b69fe1197e52cf49d3a0fca04f98cd283502ac`, Windows `7708ef018390d318072d44e85b0178d4d9bdf34b055d0342d1ae6588799dfd73`). 기존 실행 파일은 백업하고 새 파일로 엔트리만 교체했으며 기존 프로세스는 강제 종료하지 않았다. Ojak 실행 산출물은 기존 0.16.0 그대로다.
- 양 OS에서 공식 `aam omp-bridge connect`로 소유 확장을 갱신했다. Mac의 Claude Sonnet 4.6 실제 응답, Windows의 `OJAK_WINDOWS_BRIDGE_OK` 응답과 각 세션의 `ojak.admission.v1` queued → submitted → completed, 실제 bridge.log 전달 기록을 확인했다. 응답 메시지의 공급자가 `anthropic`인 것은 브릿지가 원래 공급자의 메시지를 전달하기 때문이며, 경유 증거는 journal과 브릿지 기록을 함께 사용했다.
- Windows의 실제 오류가 남아 있던 Orca 터미널에서 공식 `/restart`로 해당 omp만 같은 대화·실행 옵션으로 재시작했다. 원래 작업을 실행하거나 파일을 수정하지 않는 연결 확인 요청에 `OJAK_SESSION_OK`가 반환되고 TUI가 idle로 돌아왔음을 확인했다. 과거 오류 문구는 대화 이력에 남아 있지만 새 요청은 성공했다. 다른 터미널이나 Orca는 종료하지 않았다.
- Mac Orca 1.4.224·Windows Orca 1.4.227은 교체하지 않았다. Windows의 기존 터미널/TUI 복구는 확인했지만 Orca structured ACP 호환·전체 클라이언트 동시 전환을 검증한 것은 아니다. upstream `omp update`로 일반 배포판을 다시 설치하면 필수 호환 계약이 사라질 수 있으므로 수정본 유지가 필요하다. 공개 릴리스 게이트는 그대로 유지한다.
- 복구 도중 사용자가 V3의 “fileless 기법 탐지 / 악성코드와 유사한 행위를 수행” 경고를 보고했다. 탐지 대상·시각을 확보하지 못해 원인이나 오탐 여부는 미확정이다. 사용자의 재개 지시 후 Windows 확장 갱신과 요청 검증은 `cmd`에서 실제 실행 파일을 직접 호출했으며 PowerShell EncodedCommand·ExecutionPolicy Bypass를 사용하지 않았다. 백신 설정·예외·격리는 변경하지 않았다. 사용자가 보호를 껐다고 알렸으므로 보호 재활성화를 요청했으며, 보호가 켜진 상태의 오탐 해소를 검증했다고 주장하지 않는다.
- 이후 CI run `39095087515`의 Mac 실패는 DMG가 아니라 `reset_test_http.rs` 회귀 테스트의 `shutdown(Shutdown::Both).unwrap()`에서 발생한 `NotConnected`였다. 불완전 요청의 연결 종료는 소켓 drop만으로 충분하므로 중복 shutdown을 제거했다. 실제 다음 HTTP 응답·요청 기록 검증은 유지했으며 로컬 Mac의 `cargo test -p aam-service reset_ -- --test-threads=1` 15개가 통과했다. 테스트 코드만 변경했으며 설치 실행 파일·버전은 바꾸지 않았다.

## 0.16.0 새 디자인 Mac 배포 (2026-10-18)

- 사용자의 요청에 따라 기존 0.14/0.15 작업본의 사용 현황 개편·오른쪽 상세 패널·설정 초안 대화상자·모델 설정 점검과 Claude 조회 수정을 모두 포함했다. `bump-version.mjs 0.16.0` 후 npm package lock을 재생성하고 Cargo lock도 갱신했다.
- Rust 402개·desktop 102개·omp 확장 25개 테스트와 타입 검사, 전체 Mac 앱/DMG 빌드를 통과했다. 패키지 및 설치 앱·CLI·실행 서비스 버전은 0.16.0이다. codesign deep/strict 검증을 통과한 ad-hoc 서명이며 Apple 공증·업데이터 서명 검증은 하지 않았다.
- DMG: `target/release/bundle/dmg/Ojak_0.16.0_aarch64.dmg`, SHA-256 `5a1a1f0e3d87739ef8fcc04171e00c4cfaccb554371372c306a41fdb5ef8718e`.
- 기존 선택대로 모델 스트림·모델 대기·인증 갱신이 모두 0이며 관리 점유 세션이 없는 시점에 교체했다. 이전 설치본은 `~/Applications/Ojak.backup-0.15.2-23c5c2e5.app`으로 보존했다. 공식 service restart·integration install이 성공했고 새 앱을 실행했다.
- 실제 설치된 Ojak 네이티브 창(1180×780)을 Orca computer-use로 조작했다. 새 계정 상태·남은 한도 목록, 접힌 요청 이력, 유료 설정 요약을 시각 확인했다. Claude 행을 눌러 오른쪽 340px 고정 상세 패널이 나타나는 것을 스크린샷과 접근성 트리로 확인했다. 유료 사용 설정창을 열고 취소한 전후 서비스 정책이 동일했다. 마지막 화면은 전체 목록과 오른쪽 Claude 상세를 열어 두었다.
- 설치 후 Claude 3개 계정 모두 배포 이후 관측 시각의 한도 3개씩을 정상 조회했고 실제 소진 한도는 exhausted를 유지했다. bridge listening과 gateway 8/8 실행을 확인했다.
- Mac 로컬 설치 완료이며 Windows 빌드·설치·공개 태그·릴리스는 하지 않았다. 기존 omp·Orca 실행 파일·인증·역할 설정은 변경하지 않았다.

## 0.15.2 Mac 로컬 배포 (2026-10-18)

- 현재 설치된 0.13.2와 같은 `v0.13.2` 기반의 `/tmp/ojak-claude-hotfix` worktree(`fix/claude-usage-mac-0.15.2`)에 adapters의 Claude 조회 수정만 옮겼다. 원래 작업 트리의 0.14/0.15 UI·모델 점검 변경은 보존하고 배포에서 제외했다. 이 작업 트리의 개발 버전 0.15.1과 설치용 0.15.2 후보는 별개다.
- 후보 버전·npm/Cargo lockfile을 0.15.2로 맞췄다. Rust 기본 workspace 400개, desktop 97개, omp 확장 25개 테스트와 타입 검사·전체 Mac 앱/DMG 빌드가 통과했다. `codesign --verify --deep --strict`와 패키지 CLI·서비스 0.15.2 출력을 확인했다. ad-hoc 서명이며 Apple 공증·업데이터 서명 검증은 하지 않았다.
- DMG: `target/release/bundle/dmg/Ojak_0.15.2_aarch64.dmg`, SHA-256 `7dcf4d88f73cf0a950f476887ce0f6cd1eb387b81c6e9c10acc667876ba1b014`. 패키지 안 서비스를 격리 DB로 실행해 실제 기본 Claude 로그인의 한도 3개 조회를 확인했다.
- 사용자는 “응답 완료 후 재시작”을 선택했다. public 모델 스트림·대기 모델 요청·인증 갱신이 모두 0인 시점을 확인하고, 관리 점유 세션이 없는 상태에서 앱을 교체했다. 이전 앱은 `~/Applications/Ojak.backup-0.13.2-87991517.app`으로 보존했다. 공식 `aam service restart`·`aam integration install`을 성공시키고 새 앱을 실행했다.
- 설치 앱·CLI·실행 중 서비스가 모두 0.15.2임과 서비스 시작 시각 변경을 확인했다. 설치 후 기존 Claude 계정 3개 모두 배포 이후 관측 시각의 한도 3개씩을 반환했고 오류 안내가 없었다. 한 계정의 실제 소진 한도는 exhausted로 유지했다. 브릿지 listening·연결 정상, gateway 8/8 실행을 확인했다.
- Mac 로컬 설치만 완료했다. Windows 설치·실행 검증·공개 태그·릴리스는 하지 않았다. 기존 omp·Orca 실행 파일과 역할/인증 설정은 변경하지 않았으며 전체 클라이언트 공개 배포로 간주하지 않는다.

## 0.15.1 Claude 사용량 조회 검증 (2026-10-17)

- 앱·CLI·서비스·lockfile 버전을 0.15.1로 올렸다. `CARGO_INCREMENTAL=0 cargo test` 기본 workspace 402개가 통과했으며 debug 서비스·CLI 빌드가 성공했다. Windows 실행·설치 파일과 UI는 이번 수정 검증 대상에 포함하지 않았다.
- 설치된 0.13.2 실제 서비스를 격리 HOME·DB와 지연 CLI fixture로 실행해, 신원 확인 21초 지연이 `PROBE_TIMEOUT`·한도 없음으로 끝나는 것을 재현했다. 수정된 0.15.1 실제 서비스는 동일 조건에서 신원 확인 21초와 `/usage` 21초를 거쳐 42.4초에 정상 한도(사용률 39%)를 반영했다.
- 같은 격리 서비스에서 사용량 응답 실패 → 기존 관측 시각 유지·stale·단계/코드/경과 기록 → 10분 이내 `force:false` 조회로 신선한 관측·오류 안내 해제를 확인했다. 별도 격리 DB의 수정 서비스와 실제 기본 Claude 로그인으로 한도 3개가 known으로 조회되는 것도 확인했다. 기존 운영 DB나 로그인 파일을 복사하지 않았다.
- 초기 빌드는 디스크 공간 부족으로 중단됐다. 재생성 가능한 `target/debug/incremental` 캐시만 정리했고, 최종 검증은 incremental cache 없이 완료했다.
- 설치된 Mac 서비스는 0.13.2이며 관측 중인 세션 5개를 확인해 앱 교체·재시작·통합 변경을 하지 않았다. 소스의 다른 UI 작업도 보존했다. Windows 설치·공개 태그·릴리스는 하지 않았으며 기존 동시 전환 게이트를 유지한다.

## 0.15.0 사용 현황 UI 검증 (2026-10-17)

- `bump-version.mjs 0.15.0`으로 앱·CLI·서비스 버전을 함께 올리고 npm·Cargo lockfile을 갱신했다. 최종 코드에서 타입 검사, desktop 102개, Rust 기본 workspace 397개, omp 확장 25개 테스트가 통과했다.
- Mac의 Chromium에서 실제 `App`을 격리 IPC fixture로 렌더링했다. macOS/Windows 스타일의 1180×780·900×700, 밝은/어두운 테마, ko/en/id에서 가로 넘침 없이 목록·고정 상세·좁은 창 상세 페이지를 확인했다. 요청 기간은 요청 이력만 바꾸며, 요청 기록만 있는 계정은 사용 중 필터에 넣지 않았다.
- 계정·유료 사용 초안의 열기/선택/취소 시 쓰기 0회, 적용 시에만 정책 쓰기, 실패 시 대화상자 유지, revision 변경 시 적용 차단, 기존 세션 유지가 fixture에서 확인됐다. 같은 계정의 갱신 시 상세 스크롤 유지·다른 계정 선택 시 초기화·필터/계정 제거 시 닫기·Escape/뒤로 가기 포커스 복귀·가림·회복 알림 동기화·오프라인의 미확인 표시를 확인했다.
- 좁은 창의 숨겨진 목록 부모 때문에 선택 대화상자가 0×0으로 사라지는 문제를 재현했다. 공통 `Modal`을 앱 WebView 루트 portal로 옮긴 뒤, 목록이 숨겨진 상태에서도 선택창이 보이고 취소 후 상세로 포커스가 복귀함을 확인했다. 임시 페이지·IPC fixture·검증 스크립트·서버·브라우저 작업 공간은 정리했다.
- 최종 `npm run build`로 `target/release/bundle/macos/Ojak.app`과 `target/release/bundle/dmg/Ojak_0.15.0_aarch64.dmg`를 만들었다. CLI·서비스 실행 버전은 각각 0.15.0이다. DMG SHA-256은 `18f1ae63a69e4512cfa484a3f050f2276c5691fb781f1df7d666c92deac90f13`이다. Apple 공증은 하지 않았다.
- 읽기 전용 조회 당시 Mac 설치 앱과 실행 중인 서비스는 0.13.2였다. 관리 ACTIVE 세션은 없지만 관측 세션 5개와 최근 15분 내 사용된 브릿지 대화 18개가 있어, 기존 세션을 보존하려고 설치본 교체·서비스 재시작·전역 통합 변경을 하지 않았다.
- Windows 0.15.0 설치 파일 빌드·네이티브 실행, macOS 설치 앱의 IPC 왕복, 실제 로그인·과금·OS 알림, 서명된 동일 후보의 양 OS 업데이트는 이번 검증에 포함하지 않았다. 공개 태그·릴리스도 만들지 않았다. 기존 동시 전환·설치·서명·공개 승인 게이트는 유지한다.

## 0.14.0 모델 설정 점검 검증 (2026-10-17)

- Mac 개발 환경에서 `omp_bridge::tests` 6개(신규 모델 점검 2개 포함), desktop 테스트 97개, `npm run typecheck`, Tauri crate의 `cargo check`를 통과했다. 전체 제품 테스트·Windows 실행 검증·설치 파일 빌드는 이번 작업에서 실행하지 않았다.
- 실제 `aam omp-bridge check-models`와 설치된 omp를 연결해 사용자 설정 수정 후 `modelFallback:false, unknownSources:[]`를 확인했다. 별도 `PI_CODING_AGENT_DIR`의 예전 Gemini 참조는 미확인 모델로 검출했고, broker 없는 격리 카탈로그 조회는 `OMP_MODELS_UNAVAILABLE`로 끝났다. 실모델 추론·로그인·사용자 서비스 재시작은 하지 않았다.
- 실제 `ConnectionsView`를 브라우저에서 렌더링하고 버튼을 눌러 실제 CLI의 격리 경고·정상 결과가 표시되는 전환과 경고 스크린샷을 확인했다. 주변 연결 상태만 fixture이며 설치된 Tauri 앱의 IPC 왕복을 검증한 것은 아니다. 임시 페이지·서버는 제거했다.
- Mac 설치본·Windows 설치본은 교체하지 않았고 공개 릴리스도 하지 않았다. 기존 통합 전환·서명·두 OS 설치 검증 게이트는 유지한다.

## 0.13.2 통합 배포 준비 기록 (2026-10-17)

- Ojak Windows 후보: `target/release/bundle/nsis/Ojak_0.13.2_x64-setup.exe`, 11,061,609 bytes, SHA-256 `b0dd6ef9cb800a22ca278c2077dcc82614d69de4dd9f0ddf7d7e1ce6fb821eab`. 로컬 업데이터 서명은 없다.
- 동시 교체할 omp는 `18.12.0-ojak.1` 실행 파일(256,087,040 bytes, SHA-256 `7708ef018390d318072d44e85b0178d4d9bdf34b055d0342d1ae6588799dfd73`), Orca는 현재 설치본과 같은 upstream `v1.4.227` (`f51e82c4c26745f6a15e215e4391f533327697e2`) 기반 `1.5.0-ojak.2` Windows NSIS다(222,369,920 bytes, SHA-256 `002b2e65382218e1342c9d4298ce745247af28f2ebd0cdb9d3971ae958c64b19`).
- Orca의 최신 sealed launch composition·spawn token·fresh-session·이력 처리를 보존해 admission 패치를 재기반했다. 독립적인 깨끗한 `v1.4.227` worktree에서 patch 적용 검사, host/CLI/renderer 타입 검사와 queue·recovery·ACP/Pi·미전송 입력 관련 10개 파일 147개 테스트가 통과했다.
- Orca 네이티브 입력은 공식 `v1.4.224` Windows 배포 파일에서 가져왔다. GitHub asset digest와 실제 파일 SHA-256은 모두 `d192a7f1142b58a8c9bc855275fb37cd41dff7f152fd3cbac3acdd96331ec849`이며, 봉인된 원본 템플릿의 9개 대상 파일 해시 검증을 통과했다. 두 upstream 간 lockfile·node-pty 소스 패치·Node runtime pin의 바이트 일치를 확인하고 네이티브 파일을 수정하지 않았다. ELF의 실제 GLIBC 요구 버전을 읽고 N-API 8 초과 심볼을 거절한 뒤 슬롯을 가져왔다. 모든 OS에서 새로 컴파일하거나 실행했다고 주장하지 않는다.
- 수정한 `v1.4.227` 소스로 9개 대상 원격 JS 템플릿을 재생성하고 `ORCA_REQUIRE_ORCAD_TEMPLATE=1`로 전체 NSIS를 빌드했다. packaged main 의존성·ConPTY 프로세스 트리 소유권·daemon 진입점·CLI 5개 명령·모바일 웹·플러그인·9개 원격 대상 검증을 모두 통과했다. 원격 템플릿을 생략한 이전 개발 후보는 교체 후보로 사용하지 않는다.
- 실제 Windows orcad를 임시 HOME·프로필·loopback 포트에서 실행해 pairing → repo/worktree 생성 → terminal RPC → 실제 PTY 명령 출력 왕복 → 종료를 확인했다(`terminal round trip OK`, `PASS`). 테스트 프로세스가 남지 않았음을 확인하고 임시 HOME·worktree를 제거했다. Ojak Rust 426개·desktop 97개·omp extension 25개와 타입 검사 및 전체 Windows NSIS 빌드도 통과했다.
- Mac 클라이언트 검증은 Actions run `38991344093`(수정본 `a9e4d9f`)에서 통과했다. macOS 15·Swift 6 이상, upstream `install:release`의 x64·arm64 의존성으로 omp 실제 버전 출력, Orca 전체 Mac 패키징, 9개 원격 대상 봉인 검증, 격리 HOME의 실제 원격 터미널 왕복과 artifact 업로드를 완료했다. 초기 실패 원인은 Swift 5.10 및 x64 네이티브 의존성 누락이었으며 기능·검증을 생략하지 않고 빌드 환경을 수정했다. 이는 Developer ID 서명·공증이나 실제 사용자 Mac의 이전 버전 업그레이드 검증이 아니다.
- PR의 Windows 검사에서 쿠폰 transport-reset 회귀가 한 차례 실패했다(POST 관측 수 0). 테스트 HTTP 서버가 불완전한 연결에도 다음 응답을 소비하던 결함을 수정하고 interrupted read를 재시도한다. 불완전 연결 뒤 실제 HTTP 요청이 원래 응답을 받는 회귀와 쿠폰 socket·재시작·중복 사용 방지 검증 14개가 Windows 로컬에서 통과했다. 제품 실행 코드는 변경하지 않았으며, CI 실패와 이 결함의 인과관계는 확정하지 않는다.
- 이후 `d98c435`의 브랜치 CI는 양 OS에서 통과했지만 PR merge-result Windows CI에는 같은 POST 관측 실패가 남았다. 테스트 전용 클라이언트의 idle connection pool을 끄고 매번 새 연결로 의도한 POST 수신 후 끊김을 검사하도록 격리했다. 제품 클라이언트의 pool·retry 정책은 바꾸지 않았다. 수정 후 Windows 로컬 서비스 검사 223개와 서비스 진입점 5개가 통과했다. PR merge-result CI 성공 전에는 병합하지 않는다.
- 사용자는 점검 재부팅 대신 **현재 세션 유지**를 명시 선택했다. `SESSION_BUSY`를 무시하는 기존 서비스 교체·강제 종료·lease 삭제·재부팅은 하지 않는다. PR 필수 검토자는 `Liopark`, 현재 작업 자격은 WRITE이며 SSH 등록 호스트도 없다. Windows 기존 설치본·Mac 실기기 설치·서명된 동일 후보·공개 릴리스는 아직 미검증이다.

## 0.13.1 후보 검증 기록 (2026-10-17)

- Windows 로컬 통합 후보: `target/release/bundle/nsis/Ojak_0.13.1_x64-setup.exe`, 11,060,658 bytes, SHA-256 `06acc92b33f4f3ecd51ad3925110b7d7aff270304cda0e996db55246dba9ac86`. 업데이터 서명이 없는 로컬 빌드다.
- Rust 426개 통과(helper 단독 실행 1개 제외), desktop 97개, omp extension 25개, 타입 검사 통과. 전체 Windows NSIS 빌드 성공. 기존 Windows 전용 `STOP_GRACE` 미사용 경고는 남아 있다.
- 실제 Codex CLI 신원 검사를 사용한 격리 DB·실제 Service dispatch에서 첫 프로필 등록, 같은 identity의 다른 프로필 거절(`ACCOUNT_ALREADY_CONNECTED`), 동일 프로필 재등록 후 계정 1행 유지를 확인했다. 사용자 로그인 파일을 복사하거나 기존 서비스 DB를 변경하지 않았다.
- 실제 React 컴포넌트를 브라우저에서 렌더링해 상시 표시된 크레딧 스위치, 한 번 클릭 시 revision 4의 설정 mutation 1회, 공급자 일반 사용 제한 안내 유지, 기존 중복 프로필의 동일 계정·한도 공유 표시를 확인했다. IPC는 격리 fixture이며 실제 과금 설정은 바꾸지 않았다.
- 실제 설치 사전 검사는 `service.prepareUninstall`의 `SESSION_BUSY`로 차단됐다. 강제 종료·lease 삭제·재부팅·기존 Windows 설치본 교체는 하지 않았다. Mac 실기기와 서명된 동일 후보 설치·공개 배포는 미검증이며, 필수 리뷰와 Admin 태그·공개 승인도 필요하다. 0.12.0의 전체 클라이언트 동시 전환 요구는 유지한다.

## 0.13.0 후보 검증 기록 (2026-10-16)

- Windows 로컬 후보: `target/release/bundle/nsis/Ojak_0.13.0_x64-setup.exe`, 11,062,928 bytes, SHA-256 `9582ad7e6bd17ae3d063320be69bc2be1cd12142b8c2f72a3e1b519c6a32afda`. 업데이터 서명이 없는 로컬 빌드이며 공개용 서명 후보를 대신하지 않는다.
- 수정 소스의 `cargo test`: 426 통과, subprocess 전용 helper 1개는 단독 실행 제외(부모 회귀 테스트에서 실행). `npm run typecheck`, desktop 97개, omp extension 25개 통과. Windows 전체 NSIS 빌드 성공.
- Windows 핸들 상속 재현은 동일한 3초 분리 자식을 사용해 수정 전 캡처 완료 3,016ms, 수정 후 9ms였다. 격리 0.13.0 실제 서비스의 named-pipe RPC에서 미확인·미지원·로그인 거절·동의 없는 사용 거절·이전 submitted의 unknown 복구·같은 request ID 재사용 차단을 확인했다. 실제 `aam setup --status`는 203ms에 끝났고 서비스는 살아 있었다.
- 실제 React 초기화권 화면에서 확인란 전 사용 불가, 확인 후 단일 mutation, IPC 중단 후 읽기 전용 복구·재사용 차단, 미지원 공급자 표시를 검증했다. UI 공급자는 격리 fixture이며 실계정 쿠폰 조회·사용 성공을 뜻하지 않는다. 실제 쿠폰 소비는 0회다.
- Windows 기존 0.11.1 설치본과 실행 중인 서비스·IDE는 교체하거나 재시작하지 않았다. Mac 실기기 설치·일반 사용자 Windows 업그레이드·서명 확인·앱 안 업데이트·공개 배포는 미검증이다. 0.12.0부터의 Ojak·omp `18.12.0-ojak.1`·Orca `1.5.0-ojak.1` 동시 전환 조건을 유지한다.
- 현재 작업 자격은 저장소 WRITE이며 Admin 태그 권한이 없다. 사용할 수 있는 Mac 접속 정보도 없으므로 위 공개 게이트를 생략하지 않는다.


## 저장소 이름을 바꿀 때

현재 저장소는 `lapalai/ojak`이다. 이름을 바꾸면 다음 항목을 함께 확인한다.

| 파일 | 바꿀 내용 |
|---|---|
| `README.md`, `docs/README.*.md`, `docs/usage*.md` | 릴리스·배지·이슈·설치 스크립트 링크 |
| `install.sh`, `install.ps1` | 설치기의 `lapalai/ojak` 저장소 설정과 공식 asset URL 검증 |
| `docs/install-for-agents.md`, `llms.txt` | 에이전트 안내와 문서 URL |
| `apps/desktop/src-tauri/tauri.conf.json` | updater endpoint `https://github.com/lapalai/ojak/releases/latest/download/latest.json` |
| `.github/ISSUE_TEMPLATE/config.yml`, `SECURITY.md` | 보안 비공개 보고 주소 |
| `Cargo.toml` | `repository`, `homepage` |
| `package.json`, `apps/desktop/package.json` | `repository`, `bugs`, `homepage` |
| 이 문서, `.github/workflows/release.yml` | 배포 주소와 `github.repository` 허용 저장소 조건 |

워크플로의 저장소 허용 조건은 포크에서 서명 비밀키를 쓰지 않도록 하는 보호 장치다. 이름을 바꿔도 없애지 않는다. 새 파일에 저장소 주소를 고정하면 이 표도 갱신한다.
