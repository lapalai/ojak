# Ojak (오작)

[![License: MIT](https://img.shields.io/github/license/lapalai/ojak)](LICENSE)
[![CI](https://github.com/lapalai/ojak/actions/workflows/ci.yml/badge.svg)](https://github.com/lapalai/ojak/actions/workflows/ci.yml)

> **상태: 미리보기 / Status: pre-release, early preview.** 공개 초기라 동작과 설치 방법이 바뀔 수 있습니다. Windows는 x64 미리보기입니다.
>
> **비공식 도구입니다.** Anthropic·OpenAI·Google·xAI와 관계가 없습니다. 본인 소유 계정만 연결하세요. 공급자 약관 준수와 계정 리스크는 사용자 책임입니다. 특히 **omp 브릿지(기본 꺼짐)**는 구독 자격 증명을 로컬에서 중개하므로 공급자 약관과 충돌할 수 있습니다. 자세한 내용은 [고지](#고지).
>
> 기여 [CONTRIBUTING.md](CONTRIBUTING.md) · 보안 [SECURITY.md](SECURITY.md) · 행동 강령 [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md) · 만든 사람 [lio](https://github.com/Liopark) by [lapal](https://github.com/lapalai)

**이미 쓰는 AI 구독, 여러 계정을 한곳에서.**

본인 소유의 Claude Code·Codex 구독 계정을 연결하는 macOS 메뉴바·Windows 트레이 앱입니다. 계정별 사용량을 한눈에 확인하고, Ojak에 연결된 CLI의 새 작업에 사용할 계정을 자동으로 배정합니다.

- **기존 구독 그대로** — 이미 사용하는 본인 계정을 연결해 시작합니다.
- **계정마다 따로, 사용량은 함께** — 계정별 로그인 환경을 분리하고 사용량을 함께 확인합니다.
- **익숙한 작업 방식 그대로** — 터미널 명령 연결 후 평소처럼 `claude`·`codex`를 실행합니다.

omp 연동은 선택 기능입니다. 브릿지를 연결하고 `ojak-*` 공급자의 모델을 선택한 요청에 적용되며, 원본 공급자로 실행하는 요청은 Ojak의 계정 배정을 거치지 않습니다.

이름은 오작교에서 따왔습니다. 일 년에 한 번, 까마귀와 까치가 은하수에 다리를 놓아 견우와 직녀를 만나게 해 준다는 이야기죠. Ojak도 작업을 딱 맞는 계정으로 이어 줍니다.

한도 우회 도구가 아닙니다. 본인 명의의 계정만 쓰는 것을 전제로 합니다. 계정 공유나 한도 회피 용도로 쓰지 마세요. omp 브릿지는 기본으로 꺼져 있는 선택 기능이며 공급자 약관과 충돌할 수 있습니다([고지](#고지)).

> 사용 현황, 세션, 연결 화면의 스크린샷은 아직 없습니다.

## 요구 사항

- macOS 13 이상, Apple silicon (arm64). Intel Mac은 아직 빌드하지 않습니다.
- Windows 10 1809 이상 또는 Windows 11, 64비트. WebView2가 필요합니다(Windows 11에는 포함).
- 본인 소유의 Claude Code 또는 Codex 로그인. omp는 선택입니다. 계정을 공유하지 마세요.

## 설치

### Mac

1. [Releases](https://github.com/lapalai/ojak/releases/latest)에서 `Ojak_<버전>_aarch64.dmg`를 받아 엽니다.
2. `Ojak`을 `응용 프로그램` 폴더로 끌어다 놓습니다.
3. Ojak을 엽니다. "확인되지 않은 개발자"라며 막히면 **시스템 설정 → 개인정보 보호 및 보안 → 그래도 열기**를 누르세요. 처음 한 번만 하면 됩니다.

### Windows (x64 미리보기)

관리자로 설치하지 마세요. 우클릭의 "관리자 권한으로 실행"도 쓰지 마세요. 관리자 권한으로 설치하면 이후 일반 권한 실행이 막힐 수 있습니다.

1. [Releases](https://github.com/lapalai/ojak/releases/latest)에서 `Ojak_<버전>_x64-setup.exe`를 받아 현재 사용자로 실행합니다.
2. 이 빌드는 서명하지 않습니다. SmartScreen이 "Windows의 PC 보호"를 띄우면 **추가 정보 → 실행**을 누르세요. AhnLab V3 등 백신이 `aam.exe`나 `aam-service.exe`를 보류하면 예외로 두고 설치 파일을 다시 실행하세요.
3. **시작하기**로 서비스와 터미널 명령을 연결합니다. 새 Windows 터미널에서 `claude` 또는 `codex`를 입력하세요. `aam`도 같은 사용자 PATH에 들어갑니다.
4. 업데이트는 설정 → 정보의 "업데이트 확인"입니다. 앱 안 업데이트가 없으면 새 setup.exe를 같은 사용자로 다시 설치하세요. 실행 중인 작업이 있으면 설치가 멈춥니다. 작업을 끝낸 뒤 다시 실행하세요.
5. 제거는 **설정 → 앱 → Ojak → 제거**입니다. omp 연결을 먼저 원래대로 되돌린 뒤 서비스를 끕니다. 실행 중인 작업이 있으면 제거가 멈춥니다. 작업을 끝내거나 PC를 다시 시작한 뒤 다시 제거하세요.
6. 데이터는 `%APPDATA%\AI Account Manager`에 있습니다. 제거해도 계정·프로필·로그는 지우지 않습니다.

설치나 제거가 막히면 설치 창의 [자세히]와 `%TEMP%\Ojak-installer.log`를 보세요.

### 처음 실행하면

**Ojak 시작하기** 화면이 뜹니다. 여기서 끝까지 진행하면 준비가 끝납니다. 터미널 명령을 직접 칠 필요는 없습니다.

1. **시작하기**를 누릅니다. 백그라운드 서비스와 터미널 명령 연결이 한 번에 설정됩니다.
2. 이 컴퓨터에서 이미 Claude Code나 Codex에 로그인해 두었다면 그 계정을 자동으로 찾습니다. 없으면 **로그인** 버튼으로 공식 로그인을 진행하세요.
3. **준비 완료**가 뜨면 새 터미널 창을 열고 `claude` 또는 `codex`를 입력합니다. 이제 Ojak이 알맞은 계정을 골라 실행합니다.

계정을 더 추가하려면 **연결 → 계정 추가**를 누르세요. 어떤 계정이 쓰였는지는 **세션** 화면에서 볼 수 있습니다.

<details><summary>고급: 터미널 한 줄 설치 (Mac)</summary>

```sh
curl -fsSL https://raw.githubusercontent.com/lapalai/ojak/main/install.sh | sh
```

실행 전에 스크립트를 읽으세요. 이 저장소 Releases의 arm64 DMG만 받고 `SHA256SUMS`와 대조한 뒤 `~/Applications/Ojak.app`에 복사하고 서비스·명령 연결·zsh PATH를 설치합니다.

| 옵션 | 의미 |
|---|---|
| `--system` | `/Applications`에 설치합니다. 이 때만 sudo를 씁니다. |
| `--skip-service` | `aam service install`을 건너뜁니다. |
| `--skip-integration` | `aam integration install`을 건너뜁니다. |
| `--skip-shell` | `aam shell install`을 건너뜁니다. |
| `--clear-quarantine` | 복사한 앱의 quarantine 속성을 지웁니다. 공증이 아니며 넘기지 않으면 실행하지 않습니다. |

이 빌드는 Apple 공증을 하지 않고 adhoc 서명이라, 업데이트 뒤 macOS가 예전에 허용한 권한을 다시 물을 수 있습니다. 앱 없이 같은 준비를 하려면 `aam setup`을 실행하세요.
</details>

## 사용

- 터미널에서 평소처럼 `claude`·`codex`를 입력하면 됩니다. 한 번 시작한 대화는 끝날 때까지 같은 계정을 씁니다.
- 한도를 다 쓴 대화는 종료할 때 다른 계정에서 이어 열지 물어봅니다. 나중에 `aam continue`로도 이어 갈 수 있습니다.
- `~/.claude`·`~/.codex`의 스킬·플러그인·지침은 모든 계정에서 함께 쓰입니다.
- omp 브릿지는 선택입니다. 연결하지 않아도 사용량 화면과 CLI 실행은 동작합니다.

```sh
aam omp-broker connect && aam omp-bridge connect
```

연결하면 실행 중인 omp 계정의 Ojak 공급자(`ojak-*`)가 자동으로 로그인됩니다. 새 omp 세션에서 `/model`로 `ojak-*` 모델을 고르세요. omp에서 계정을 나중에 추가해도 약 1분 안에 맞춰집니다.

브릿지는 구독 자격 증명을 로컬 프록시로 통과시킵니다. 공급자 약관과 충돌할 수 있습니다. 아래 [고지](#고지)를 읽으세요.

### 계정이 배정되지 않을 때

`claude`나 `codex`를 실행했는데 다음 메시지가 나오면, 관리 배정 없이 원래 CLI로 실행된 것입니다. 이 실행은 관리 세션으로 기록되지 않습니다.

```text
aam: 관리 배정을 적용하지 못해 원본 claude을(를) 그대로 실행합니다. ... (NO_ELIGIBLE_ACCOUNT)
```

계정별 제외 이유는 다음 명령으로 확인합니다. 계정을 예약하지 않습니다.

```sh
aam explain --tool claude
```

| 이유 | 뜻 |
|---|---|
| `SAFETY_RESERVE` | 사용량이 안전 잔여량 아래로 내려가 새 배정을 멈췄습니다. 한도가 초기화되면 다시 배정됩니다. |
| `MODEL_LIMIT_GUARD` | 특정 모델 전용 한도가 소진됐습니다. 공식 CLI가 모델을 고르는 요청은 막지 않습니다. |
| `CAPACITY_RESERVED` | 이 계정의 동시 실행 자리를 다른 세션이 쓰고 있거나, 그 세션이 끝났는지 확인되지 않았습니다. |

#### 한도를 다 써서 대화가 끊겼을 때 (Claude Code·Codex)

Ojak으로 실행한 Claude Code·Codex 창에서 한도를 다 써서 나오면, 다른 계정에 여유가 있을 때 이렇게 묻습니다.

```text
aam: 이 계정은 한도를 다 써서 새 작업을 받을 수 없습니다. 같은 대화를 ○○ 계정에서 이어 열까요? [Y/n]
```

Enter를 누르면 같은 대화가 다른 계정으로 다시 열립니다. 대화 기록만 옮기고 로그인은 건드리지 않습니다. 묻지 않았다면(사용량 관측이 아직 늦은 경우) 같은 폴더에서 `aam continue`를 실행하세요. 캐시가 새로 만들어지므로 이어 연 첫 요청은 사용량이 조금 더 듭니다. 이어 받는 계정을 대화형으로 처음 쓰는 경우 Claude Code의 첫 실행 안내가 한 번 나올 수 있습니다.

#### 세션이 끝났는데 자리가 풀리지 않을 때

Ojak은 세션이 확실히 끝났다고 확인될 때만 자리를 돌려줍니다. 시간이 지났다는 이유만으로는 풀지 않습니다. 끝났다고 잘못 판단해 같은 계정에 세션을 더 넣으면, 동시 실행 제한을 넘거나 계정이 섞일 수 있기 때문입니다.

세션 화면의 상태로 구분합니다.

- **생존 확인 중(`SUSPECT`)**: 실행 결과나 생존 여부를 아직 확인하지 못했습니다. 실행기가 살아 있으면 보통 스스로 정리됩니다.
- **관리 연결 유실(`ORPHANED`)**: CLI는 끝났지만, 실행기가 종료 보고를 하기 전에 사라졌습니다. 이때는 CLI가 남긴 자손 프로세스가 없는지 증명할 근거가 없어 자리를 계속 잡습니다.

자주 생기는 원인은 다음과 같습니다.

- 다른 도구(MCP 서버, 에디터 확장 등)가 띄운 `claude -p`가 일시정지(`T` 상태)된 채 남아 있습니다.
- 실행기(`aam`)를 `kill -9`로 강제 종료했습니다.

일시정지된 작업이 남아 있으면 먼저 그 작업을 정상적으로 끝내세요. 정지된 프로세스는 종료 신호를 보내도 다시 움직일 때까지 처리하지 못하므로, 종료 신호 뒤에 `CONT`를 함께 보냅니다.

```sh
ps -o pid,ppid,stat,command -ax | grep '[c]laude -p'   # STAT에 T가 있으면 일시정지 상태
kill -TERM <pid> && kill -CONT <pid>
```

실행기가 종료 보고와 자손 정리를 마치면 자리가 반환됩니다. 실행기까지 함께 끝나 버리면 보고가 빠져 `ORPHANED`로 남을 수 있습니다.

`ORPHANED` 세션 중 자손 프로세스 기록이 있는 세션은 기록된 자손과 원래 프로세스 그룹이 모두 끝난 것이 확인되면 자동으로 풀립니다. 자손 기록이 없는 세션은 같은 부팅 안에서는 남은 자손이 없다는 것을 증명할 수 없어 계속 자리를 잡습니다. 재부팅하면 모든 프로세스가 끝나므로, 재부팅 뒤 서비스가 시작될 때 자리를 반환합니다. 급하지 않다면 재부팅을 기다리세요. 서비스 DB를 직접 고치지 마세요. 그 계정은 제외된 채로 나머지 계정에 배정이 계속됩니다.

## 업데이트

서명 공개키가 설정된 설치본은 시작 시와 24시간마다 업데이트를 확인하고, 설정 → 정보의 "업데이트 확인"과 메뉴바·트레이 팝오버의 배지로 알려 줍니다. Mac은 앱을 바꾼 뒤 `ai.aam.service` LaunchAgent를 다시 시작합니다. Windows는 로그인 시 자동 실행으로 등록된 서비스를 다시 시작합니다. 어느 쪽이든 실행 중인 omp 브릿지 세션은 잠깐 끊깁니다. 관리 세션이 있거나 최근 15분 안에 브릿지 요청이 있으면 "지금 업데이트"와 "나중에" 중 고릅니다.

공개키가 비어 있거나 `REPLACE_WITH_TAURI_UPDATER_PUBKEY`이면 업데이트 UI는 나타나지 않습니다. 지금 저장소의 공개키는 그 자리표시자가 아닙니다. 앱 안 업데이트가 없으면 새 DMG를 받거나, Windows에서는 새 setup.exe를 같은 사용자로 다시 설치하세요.

## 제거

Mac:

```sh
aam deactivate
```

계정, 프로필, 로그는 남고 Ojak이 넣은 연결만 되돌립니다. 앱 묶음까지 지우려면:

```sh
sh install.sh --uninstall
```

`/Applications`에 설치했다면 `--uninstall --system`입니다. sudo는 그 경우에만 씁니다.

Windows는 **설정 → 앱 → Ojak → 제거**입니다. 위의 [Windows](#windows-x64-미리보기) 절을 보세요. `aam deactivate`도 같은 연결 해제를 합니다.

## 개인정보

Ojak이 이 컴퓨터에 두는 데이터는 Mac에서 `~/Library/Application Support/AI Account Manager`, Windows에서 `%APPDATA%\AI Account Manager`입니다. SQLite의 계정·배정 기록, `integration.json`, `ui-settings.json`, 진단용 로그가 있습니다. Ojak 자신은 사용 통계를 보내거나 진단 파일을 업로드하지 않습니다.

공식 CLI와, 켠 경우에만, omp 브릿지는 본인 로그인으로 해당 공급자 API에 요청을 보냅니다. 그 통신은 Ojak의 수집이 아닙니다.

## 고지

Ojak은 비공식 프로젝트입니다. Anthropic, OpenAI, Google, xAI와 관계가 없습니다.

본인 계정만 사용하세요. 계정을 공유하지 마세요.

omp 브릿지는 구독 자격 증명을 로컬 프록시로 통과시킵니다. 이 사용은 공급자 약관과 충돌할 수 있습니다.

- https://code.claude.com/docs/en/legal-and-compliance
- https://openai.com/policies/row-terms-of-use/

사용에 대한 책임은 사용자에게 있습니다. 계정 정지가 있을 수 있습니다. 이 문서는 적법성이나 안전성을 주장하지 않습니다.

## 소스에서 빌드

Rust stable과 Node.js 22가 필요합니다.

```sh
npm ci
npm run build
```

결과: `target/release/bundle/macos/Ojak.app`, `target/release/bundle/dmg/`.

서명 비밀키가 없으면 `scripts/build.mjs`는 updater 아티팩트만 끄고 앱과 DMG는 만듭니다. 키가 있으면 `Ojak.app.tar.gz`와 `.sig`도 만듭니다. 개인 키는 저장소에 넣지 마세요.

버전은 세 파일을 같이 올립니다.

```sh
node scripts/bump-version.mjs 0.2.0
```

`x.y.z`가 아니면 거부합니다. 태그는 `v0.2.0`처럼 그 버전과 같아야 합니다.

언제·어떤 버전을 내는지, 태그 전 확인 목록과 되돌리기는 [docs/release-policy.md](docs/release-policy.md)를, 변경 내용은 [CHANGELOG.md](CHANGELOG.md)를 따릅니다.

## 저장소 이름

저장소는 `lapalai/ojak`입니다. 이름을 바꾸면 아래를 함께 고칩니다.

| 파일 | 바꿀 값 |
|---|---|
| 이 README | 설치 명령, 라이선스·CI 배지의 `lapalai/ojak` |
| `install.sh` | `OJAK_REPO=lapalai/ojak` |
| `apps/desktop/src-tauri/tauri.conf.json` | updater endpoint `https://github.com/lapalai/ojak/releases/latest/download/latest.json` |
| `.github/ISSUE_TEMPLATE/config.yml` | 보안 신고 주소 `https://github.com/lapalai/ojak/security/advisories/new` |
| `SECURITY.md` | 비공개 보고 주소 `https://github.com/lapalai/ojak/security/advisories/new` |
| `Cargo.toml` | `repository`, `homepage` |
| `package.json`, `apps/desktop/package.json` | `repository`, `bugs`, `homepage` |

릴리스 워크플로는 `github.repository`를 씁니다. 새 파일에 저장소 이름을 하드코드하면 이 표에 추가하세요.

## 업데이트 서명 키

업데이터 서명은 Apple 서명과 별개입니다. 개인 키를 저장소에 만들거나 커밋하지 마세요.

```sh
npx tauri signer generate -w "$HOME/.tauri/ojak.key"
```

1. 출력된 공개키를 `tauri.conf.json`의 `plugins.updater.pubkey`에 넣습니다. 지금 커밋된 값은 자리표시자가 아니라 실제 minisign 공개키입니다. `REPLACE_WITH_TAURI_UPDATER_PUBKEY`로 되돌리면 앱은 업데이트 UI를 숨기고 확인 요청을 보내지 않습니다.
2. 그 공개키와 한 쌍인 개인 키 내용을 GitHub secret `TAURI_SIGNING_PRIVATE_KEY`에 넣습니다. 비밀키는 저장소에 넣지 말고, 저장소 밖에도 보관합니다.
3. 키에 암호를 줬다면 `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`에도 넣습니다. 없으면 빈 값으로 둡니다.
4. 공개키와 비밀키는 한 쌍이어야 합니다. 릴리스 워크플로는 빌드 전에 `scripts/verify-signing-key.mjs`로 작은 파일을 비밀키로 서명해 저장소의 공개키로 검증합니다. 짝이 다르거나 암호가 틀리거나 비어 있으면 거기서 멈춥니다. 로컬 확인: `TAURI_SIGNING_PRIVATE_KEY="$(cat ~/.tauri/ojak-updater.key)" TAURI_SIGNING_PRIVATE_KEY_PASSWORD=... node scripts/verify-signing-key.mjs`

공개키는 이미 저장소에 있습니다. 첫 태그 전에 짝이 되는 비밀키가 GitHub secret에 있어야 그 다음 버전부터 앱 안 업데이트가 됩니다.

## 만든 사람

[lio](https://github.com/Liopark) by [lapal](https://github.com/lapalai)

## 라이선스

[MIT](LICENSE). Copyright (c) 2026 lapal. 제3자 고지: [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).

---


# Bahasa Indonesia

Ojak adalah aplikasi menu bar macOS dan tray Windows. Kamu bisa lihat pemakaian beberapa akun langganan AI dalam satu layar, lalu jalankan CLI resmi (Claude Code, Codex) dengan akun yang pas. Ojak bukan alat untuk mengakali batas pemakaian.

Namanya dari *ojak* (오작), kawanan gagak dan murai dalam dongeng Korea. Setahun sekali mereka membuat jembatan di Bima Sakti supaya dua kekasih yang terpisah bisa bertemu. Ojak juga begitu: kerjaanmu sampai ke akun yang pas.

Pakai hanya akun milikmu. Jangan dipakai untuk berbagi akun. Sambungan omp opsional, mati secara bawaan, dan bisa bentrok dengan ketentuan penyedia. Ojak proyek tidak resmi dan tidak berafiliasi dengan Anthropic, OpenAI, Google, maupun xAI.

Bahasa Indonesia sudah ada di aplikasi (Pengaturan → Bahasa). Panduan pasang dan pakai lengkap ada di bagian [English](#english) di bawah.

Status: pratinjau awal (pre-release). [Kontribusi](CONTRIBUTING.md) · [Keamanan](SECURITY.md) · [Kode etik](CODE_OF_CONDUCT.md).

# English

Ojak is a macOS menu-bar and Windows tray app: a multi-account usage view and a per-account CLI launcher. It is not a limit-bypass tool.

The name comes from *ojak* (오작), the crows and magpies of a Korean folk tale. Once a year they form a bridge across the Milky Way so two separated lovers can meet. Ojak does much the same for your work: it gets it to the right account.

It assumes you only use accounts you own. Do not use it to share accounts or evade limits. The omp bridge is an optional feature, off by default, and may conflict with provider terms ([disclaimer](#disclaimer)).

> **Status: pre-release / early preview.** Behavior and install steps may change. Windows is an x64 preview.
>
> **Unofficial tool.** Not affiliated with Anthropic, OpenAI, Google, or xAI. Connect only accounts you own; compliance with provider terms and any account risk are yours. The optional omp bridge (off by default) relays subscription credentials locally and may conflict with provider terms ([disclaimer](#disclaimer)).
>
> [Contributing](CONTRIBUTING.md) · [Security](SECURITY.md) · [Code of Conduct](CODE_OF_CONDUCT.md) · Made by [lio](https://github.com/Liopark) by [lapal](https://github.com/lapalai)

> Screenshots of the usage, sessions, and connections views are not in this README yet.

## Requirements

- macOS 13 or later, Apple silicon (arm64). Intel Macs are not built yet.
- Windows 10 1809 or later, or Windows 11, 64-bit. WebView2 is required (included on Windows 11).
- Your own Claude Code or Codex login. omp is optional. Do not share accounts.

## Install

### Mac

1. Download `Ojak_<version>_aarch64.dmg` from [Releases](https://github.com/lapalai/ojak/releases/latest) and open it.
2. Drag `Ojak` into `Applications`.
3. Open Ojak. If macOS blocks it as from an unidentified developer, go to **System Settings → Privacy & Security → Open Anyway**. You only do this once.

### Windows (x64 preview)

Do not run the installer as administrator, including "Run as administrator" from the right-click menu. An admin install can block later launches as a normal user.

1. Download `Ojak_<version>_x64-setup.exe` from [Releases](https://github.com/lapalai/ojak/releases/latest) and run it as the current user.
2. This build is unsigned. If SmartScreen says "Windows protected your PC", choose **More info → Run anyway**. If antivirus software such as AhnLab V3 quarantines `aam.exe` or `aam-service.exe`, allow those files and run the installer again.
3. Press **Start** to connect the service and terminal commands. In a new Windows Terminal window, type `claude` or `codex`. `aam` is added to the same user PATH.
4. Updates are under Settings → About → Check for updates. If in-app update is unavailable, install a new setup.exe as the same user. Installation stops while work is running. Finish that work, then run the installer again.
5. Uninstall from **Settings → Apps → Ojak → Uninstall**. Ojak restores omp first, then stops the service. Uninstall stops while work is running. Finish that work or restart the PC, then uninstall again.
6. Data stays in `%APPDATA%\AI Account Manager`. Uninstall does not delete accounts, profiles, or logs.

If install or uninstall stops, open Details in the installer window and `%TEMP%\Ojak-installer.log`.

### First launch

The **Get started with Ojak** screen appears. Finish it and you're set. No terminal commands needed.

1. Press **Start**. The background service and terminal commands are set up in one step.
2. If you already signed in to Claude Code or Codex on this computer, Ojak finds that account. Otherwise use **Sign in** for the official login.
3. When **You're all set** appears, open a new terminal and type `claude` or `codex`. Ojak now picks the right account.

Add more accounts under **Connections → Add account**. The **Sessions** view shows which account was used.

<details><summary>Advanced: one-line terminal install (Mac)</summary>

```sh
curl -fsSL https://raw.githubusercontent.com/lapalai/ojak/main/install.sh | sh
```

Read the script first. It accepts only an arm64 DMG from this repository's Releases, checks it against `SHA256SUMS`, copies it to `~/Applications/Ojak.app`, and installs the service, command integration and zsh PATH.

| Option | Meaning |
|---|---|
| `--system` | Install into `/Applications`. Only this uses sudo. |
| `--skip-service` | Skip `aam service install`. |
| `--skip-integration` | Skip `aam integration install`. |
| `--skip-shell` | Skip `aam shell install`. |
| `--clear-quarantine` | Remove the quarantine attribute from the copied app. Not notarization; off unless passed. |

The build is not notarized and is ad-hoc signed, so macOS may ask again for permissions after an update. To do the same setup without the app, run `aam setup`.
</details>

## Usage

- Type `claude` or `codex` in a terminal as usual. A conversation keeps the same account until it ends.
- When a conversation runs out of limit, Ojak offers to continue it on another account as you exit. `aam continue` does the same later.
- Skills, plugins and instructions in `~/.claude` and `~/.codex` are shared by every account.
- The omp bridge is optional. Usage and CLI launches work without it.

```sh
aam omp-broker connect && aam omp-bridge connect
```

Connecting logs in an Ojak provider (`ojak-*`) automatically for each running omp account. Open a new omp session and pick an `ojak-*` model with `/model`. An account added later in omp is picked up within about a minute.

The bridge passes subscription credentials through a local proxy and may conflict with provider terms. Read the [Disclaimer](#disclaimer).

### When no account is assigned

If `claude` or `codex` prints the message below, it ran the original CLI without managed assignment. That run is not recorded as a managed session.

```text
aam: ... (NO_ELIGIBLE_ACCOUNT)
```

See why each account was excluded. This does not reserve an account.

```sh
aam explain --tool claude
```

| Reason | Meaning |
|---|---|
| `SAFETY_RESERVE` | Usage fell below the safety reserve, so new assignments stopped. They resume after the limit resets. |
| `MODEL_LIMIT_GUARD` | A model-specific limit is used up. Requests where the official CLI picks the model are not blocked. |
| `CAPACITY_RESERVED` | Another session is using this account's concurrency slot, or Ojak could not confirm that session ended. |

#### A conversation stopped because the limit ran out (Claude Code, Codex)

When a Claude Code or Codex window launched through Ojak ends because its account ran out, and another account has room, Ojak asks:

```text
aam: ... continue the same conversation on another account? [Y/n]
```

Press Enter and the same conversation reopens on that account. Only the conversation file moves; sign-ins are untouched. If Ojak did not ask (usage data can lag), run `aam continue` in the same folder. The first request after continuing costs a bit more because the prompt cache is rebuilt. If the receiving account was never used interactively, Claude Code may show its first-run screens once.

#### A session ended but its slot stays taken

Ojak returns a slot only when it can confirm the session ended. It never frees one just because time passed. Treating a live session as finished could put more sessions on the same account than its concurrency limit allows, or mix accounts.

The session state in the Sessions view tells you which case you are in:

- **Checking liveness (`SUSPECT`)**: the start result or liveness is not confirmed yet. If the launcher is alive, this usually resolves on its own.
- **Lost supervision (`ORPHANED`)**: the CLI ended, but the launcher disappeared before reporting the exit. Ojak cannot prove that no descendant processes were left behind, so it keeps the slot.

Common causes:

- A `claude -p` started by another tool (an MCP server, an editor extension) is left suspended (`T` state).
- The launcher (`aam`) was killed with `kill -9`.

If a suspended job is left over, end it normally first. A stopped process cannot handle a termination signal until it resumes, so send `CONT` right after `TERM`:

```sh
ps -o pid,ppid,stat,command -ax | grep '[c]laude -p'   # T in STAT means suspended
kill -TERM <pid> && kill -CONT <pid>
```

The slot is returned once the launcher reports the exit and finishes cleaning up descendants. If the launcher ends too, that report is lost and the session can stay `ORPHANED`.

An `ORPHANED` session with recorded descendants is released automatically once all recorded descendants and the original process group are confirmed gone. A session without descendant records cannot prove that nothing was left behind within the same boot, so it keeps the slot. A reboot ends every process, so the slot is returned when the service starts after the reboot. Do not edit the service database. Ojak keeps assigning the remaining accounts and leaves that one out.

## Updates

When the signing public key is configured, the app checks on startup and every 24 hours. Settings → About has "Check for updates". The menu-bar or tray popover shows a small badge when an update is waiting. On Mac, installing replaces the app and restarts the `ai.aam.service` LaunchAgent. On Windows, it restarts the service registered to start at sign-in. Either restart briefly interrupts omp bridge sessions. If a managed session is in use, or a bridge session was used in the last 15 minutes, the app asks you to choose "Update now" or "Later".

If the public key is empty or still `REPLACE_WITH_TAURI_UPDATER_PUBKEY`, the update UI is hidden and no update request is made. The key committed in this repository is not that placeholder. If in-app update is unavailable, install a new DMG, or on Windows a new setup.exe.

## Uninstall

On Mac:

```sh
aam deactivate
```

This restores the connections Ojak added and keeps accounts, profiles, and logs. To remove the app bundle as well:

```sh
sh install.sh --uninstall
```

A copy in `/Applications` needs `--uninstall --system`. sudo is used only in that case.

On Windows, use **Settings → Apps → Ojak → Uninstall**. See [Windows](#windows-x64-preview). `aam deactivate` performs the same connection restore.

## Privacy

Ojak stores its data on this computer under `~/Library/Application Support/AI Account Manager` on Mac and `%APPDATA%\AI Account Manager` on Windows: SQLite account and allocation records, `integration.json`, `ui-settings.json`, and diagnostic logs. Ojak itself does not send usage telemetry or upload diagnostics.

The official CLIs, and the omp bridge only if you connect it, send requests to those providers with your own login. That traffic is not collected by Ojak.

## Disclaimer

Ojak is unofficial. It is not affiliated with Anthropic, OpenAI, Google, or xAI.

Use only your own accounts. Do not share accounts.

The omp bridge routes subscription credentials through a local proxy and may conflict with provider terms:

- https://code.claude.com/docs/en/legal-and-compliance
- https://openai.com/policies/row-terms-of-use/

You are responsible for how you use it. Account suspension is possible. This document does not claim that use is legal or safe.

## Build from source

Rust stable and Node.js 22.

```sh
npm ci
npm run build
```

Output: `target/release/bundle/macos/Ojak.app` and `target/release/bundle/dmg/`.

Without a signing private key, `scripts/build.mjs` turns off updater artifacts and still builds the app and DMG. With the key, it also writes `Ojak.app.tar.gz` and `.sig`. Do not commit a private key.

Bump the version in lockstep:

```sh
node scripts/bump-version.mjs 0.2.0
```

Values that are not `x.y.z` are refused. The git tag must be `v` plus that version.

When to release, which version to bump, the pre-tag checklist and rollback rules are in [docs/release-policy.md](docs/release-policy.md) (Korean). Changes go in [CHANGELOG.md](CHANGELOG.md).

## Repository name

The repository is `lapalai/ojak`. If it moves, update these together.

| File | Replace |
|---|---|
| This README | install command, and `lapalai/ojak` in the license and CI badges |
| `install.sh` | `OJAK_REPO=lapalai/ojak` |
| `apps/desktop/src-tauri/tauri.conf.json` | updater endpoint `https://github.com/lapalai/ojak/releases/latest/download/latest.json` |
| `.github/ISSUE_TEMPLATE/config.yml` | security report URL `https://github.com/lapalai/ojak/security/advisories/new` |
| `SECURITY.md` | private report URL `https://github.com/lapalai/ojak/security/advisories/new` |
| `Cargo.toml` | `repository`, `homepage` |
| `package.json`, `apps/desktop/package.json` | `repository`, `bugs`, `homepage` |

The release workflow uses `github.repository`. If a new file hard-codes the repository name, add it to this table.

## Updater signing key

Updater signing is separate from Apple signing. Do not generate or store the private key in the repo.

```sh
npx tauri signer generate -w "$HOME/.tauri/ojak.key"
```

1. Put the printed public key in `plugins.updater.pubkey` in `tauri.conf.json`. The committed value is a real minisign public key, not a placeholder. Putting `REPLACE_WITH_TAURI_UPDATER_PUBKEY` back hides the update UI and stops update requests.
2. Put the matching private key contents in the GitHub secret `TAURI_SIGNING_PRIVATE_KEY`. Do not commit the private key, and keep a copy outside the repository.
3. If the key has a password, also set `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`. Otherwise leave that secret empty.
4. The public and private keys must be a pair. Before building, the release workflow runs `scripts/verify-signing-key.mjs`: it signs a small file with the secret and verifies it against the committed public key. A mismatched pair, wrong password, or empty secret stops the workflow there. Local check: `TAURI_SIGNING_PRIVATE_KEY="$(cat ~/.tauri/ojak-updater.key)" TAURI_SIGNING_PRIVATE_KEY_PASSWORD=... node scripts/verify-signing-key.mjs`

The public key is already in the repository. The matching private key must be in the GitHub secret before the first tag, or later in-app updates cannot be verified.

## Made by

[lio](https://github.com/Liopark) by [lapal](https://github.com/lapalai)

## License

[MIT](LICENSE). Copyright (c) 2026 lapal. Third-party notices: [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).
